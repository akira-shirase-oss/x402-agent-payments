//! The payment layer. See the crate docs for the flow; `docs/DECISIONS.md` D-005..D-008 for
//! why it is shaped this way.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use alloy_primitives::{Address, U256};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tower::{Layer, Service, ServiceExt as _};
use xpay_x402::clock::Clock;
use xpay_x402::exact;
use xpay_x402::facilitator::{BoxFuture, Facilitator, FacilitatorError};
use xpay_x402::{
    FacilitatorRequest, HeaderError, PAYMENT_REQUIRED_HEADER, PAYMENT_RESPONSE_HEADER,
    PAYMENT_SIGNATURE_HEADER, PaymentPayload, PaymentRequired, PaymentRequirements, ResourceInfo,
    SCHEME_EXACT, SettleResponse, X402_VERSION, decode_header, encode_header, reason,
};

use crate::store::{Claim, ClaimState, Delivery, NonceKey, NonceStore};

/// How far past `now + maxTimeoutSeconds` a `validBefore` may reach (buyer clock skew).
/// Bounding it bounds how long a claim must be remembered.
pub const VALIDITY_SKEW_SECS: u64 = 30;

/// When the handler runs relative to settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettleMode {
    /// Settle, then serve: nothing is served for money that has not moved. The default.
    #[default]
    SettleFirst,
    /// Verify, serve, then settle; if settlement fails the handler's response is dropped
    /// and the client gets a 402. The nonce is still claimed first, so one payment cannot
    /// run the handler twice. For handlers that are cheap and idempotent.
    SettleAfter,
}

#[derive(Debug, Clone)]
pub struct PaymentConfig {
    pub resource: ResourceInfo,
    /// The offers, in preference order (`exact::usdc_requirements`).
    pub accepts: Vec<PaymentRequirements>,
    pub mode: SettleMode,
    pub verify_timeout: Duration,
    pub settle_timeout: Duration,
    /// Deadline on the wrapped handler once a payment is behind it.
    pub handler_timeout: Duration,
    /// Bound: payments past the offline checks at once, counting their facilitator calls
    /// and handler runs until they end; more get 503.
    pub max_in_flight: usize,
    /// Bound: of those, how many one client (IPv4 address or IPv6 /64) may hold.
    pub max_in_flight_per_client: usize,
    /// Peers whose right-most `X-Forwarded-For` hop names the client.
    pub trusted_proxies: Vec<IpAddr>,
    /// If set, the layer takes an exclusive lock file here per offer, so a second seller
    /// process for the same `payTo` on this host refuses to start (D-017).
    pub lock_dir: Option<PathBuf>,
}

impl PaymentConfig {
    pub fn new(resource: ResourceInfo, accepts: Vec<PaymentRequirements>) -> Self {
        Self {
            resource,
            accepts,
            mode: SettleMode::SettleFirst,
            verify_timeout: Duration::from_secs(5),
            settle_timeout: Duration::from_secs(20),
            handler_timeout: Duration::from_secs(30),
            max_in_flight: 256,
            max_in_flight_per_client: 16,
            trusted_proxies: Vec::new(),
            lock_dir: None,
        }
    }
}

/// Why a [`PaymentLayer`] cannot be built.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("no offers")]
    NoOffers,
    #[error("offer {0} is not exact USDC on a known network with its real domain")]
    Offer(usize),
    #[error("a bound is zero")]
    ZeroBound,
    #[error("offer {0}'s payTo already has a nonce store in this process")]
    PayToTaken(usize),
    #[error("offer {0}'s payTo is locked by another seller process")]
    PayToLocked(usize),
    #[error("lock file: {0}")]
    Lock(String),
}

/// What the handler learns about the payment: `Extension<Paid>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paid {
    pub payer: Address,
    pub amount: U256,
    /// The settlement transaction (`None` in settle-after mode, before settlement).
    pub transaction: Option<String>,
}

/// One payee in one process: (network, asset, payTo).
type Payee = (String, Address, Address);

/// The process's payees and the store each one's claims live in (D-017): two stores for
/// one payee would each accept the same authorisation once.
static PAYEES: LazyLock<Mutex<HashMap<Payee, (usize, usize)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn payees() -> std::sync::MutexGuard<'static, HashMap<Payee, (usize, usize)>> {
    PAYEES.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Shared {
    config: PaymentConfig,
    facilitator: Arc<dyn Facilitator>,
    store: Arc<dyn NonceStore>,
    clock: Arc<dyn Clock>,
    gate: Arc<Semaphore>,
    clients: Arc<Clients>,
    payees: Vec<Payee>,
    /// Held for the layer's life.
    _locks: Vec<std::fs::File>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        let mut all = payees();
        for p in &self.payees {
            if let Some((_, n)) = all.get_mut(p) {
                *n -= 1;
                if *n == 0 {
                    all.remove(p);
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct PaymentLayer {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for PaymentLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaymentLayer")
            .field("config", &self.shared.config)
            .finish_non_exhaustive()
    }
}

/// The payee an offer pays, if it is exact USDC as `exact::USDC` lists it.
fn payee(o: &PaymentRequirements) -> Option<Payee> {
    let usdc = exact::usdc(&o.network)?;
    let extra = o.extra.as_ref()?;
    let ok = o.scheme == SCHEME_EXACT
        && o.asset.eq_ignore_ascii_case(usdc.address)
        && extra.get("name").and_then(|v| v.as_str()) == Some(usdc.domain_name)
        && extra.get("version").and_then(|v| v.as_str()) == Some(usdc.domain_version)
        && o.max_timeout_seconds > 0
        && U256::from_str_radix(&o.amount, 10).is_ok_and(|a| !a.is_zero());
    if !ok {
        return None;
    }
    let asset = Address::from_str(&o.asset).ok()?;
    let pay_to = Address::from_str(&o.pay_to).ok()?;
    Some((o.network.as_str().to_owned(), asset, pay_to))
}

fn lock(dir: &std::path::Path, p: &Payee, i: usize) -> Result<std::fs::File, ConfigError> {
    let name = format!("x402-seller-{}-{:x}.lock", p.0.replace(':', "-"), p.2);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(name))
        .map_err(|e| ConfigError::Lock(e.kind().to_string()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(ConfigError::PayToLocked(i)),
        Err(std::fs::TryLockError::Error(e)) => Err(ConfigError::Lock(e.kind().to_string())),
    }
}

impl PaymentLayer {
    /// Checks every offer against `exact::USDC` and claims each payee for `store`: one
    /// store per payee in a process, and with `lock_dir` one process per payee on a host.
    pub fn new(
        config: PaymentConfig,
        facilitator: Arc<dyn Facilitator>,
        store: Arc<dyn NonceStore>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ConfigError> {
        if config.accepts.is_empty() {
            return Err(ConfigError::NoOffers);
        }
        if config.max_in_flight == 0 || config.max_in_flight_per_client == 0 {
            return Err(ConfigError::ZeroBound);
        }
        let mut wanted: Vec<Payee> = Vec::new();
        for (i, o) in config.accepts.iter().enumerate() {
            let p = payee(o).ok_or(ConfigError::Offer(i))?;
            if !wanted.contains(&p) {
                wanted.push(p);
            }
        }
        let mut locks = Vec::new();
        if let Some(dir) = &config.lock_dir {
            for (i, p) in wanted.iter().enumerate() {
                locks.push(lock(dir, p, i)?);
            }
        }
        let id = Arc::as_ptr(&store).cast::<()>() as usize;
        {
            let mut all = payees();
            if let Some(i) = wanted
                .iter()
                .position(|p| all.get(p).is_some_and(|(owner, _)| *owner != id))
            {
                return Err(ConfigError::PayToTaken(i));
            }
            for p in &wanted {
                all.entry(p.clone()).or_insert((id, 0)).1 += 1;
            }
        }
        let gate = Arc::new(Semaphore::new(config.max_in_flight));
        let clients = Arc::new(Clients {
            per: config.max_in_flight_per_client,
            held: Mutex::new(HashMap::new()),
        });
        Ok(Self {
            shared: Arc::new(Shared {
                config,
                facilitator,
                store,
                clock,
                gate,
                clients,
                payees: wanted,
                _locks: locks,
            }),
        })
    }
}

impl<S> Layer<S> for PaymentLayer {
    type Service = PaymentService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        PaymentService {
            inner,
            shared: Arc::clone(&self.shared),
        }
    }
}

#[derive(Clone)]
pub struct PaymentService<S> {
    inner: S,
    shared: Arc<Shared>,
}

impl<S> std::fmt::Debug for PaymentService<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaymentService").finish_non_exhaustive()
    }
}

impl<S> Service<Request<Body>> for PaymentService<S>
where
    S: Service<Request<Body>, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let ready = self.inner.clone();
        let inner = std::mem::replace(&mut self.inner, ready);
        let shared = Arc::clone(&self.shared);
        Box::pin(async move { Ok(handle(shared, req, inner).await) })
    }
}

/// The outcome of a facilitator call that ended the payment without a settlement.
enum Outcome {
    /// Definitely not paid: the claim was released.
    Refused {
        reason: String,
        settle: Option<Box<SettleResponse>>,
    },
    /// Facilitator unreachable before anything could move: the claim was released.
    Unavailable(StatusCode),
    /// Settlement outcome unknown: the claim is held.
    Held(StatusCode, &'static str),
}

/// Settlement failures that happen before anything is broadcast, so the authorisation is
/// still unspent and may be retried (D-009). Anything else, and any failure that names a
/// transaction, holds the claim.
const RELEASE_ON_SETTLE: [&str; 12] = [
    reason::INSUFFICIENT_FUNDS,
    reason::VALID_AFTER,
    reason::VALID_BEFORE,
    reason::VALUE_MISMATCH,
    reason::SIGNATURE,
    reason::RECIPIENT_MISMATCH,
    reason::NETWORK,
    reason::NETWORK_MISMATCH,
    reason::PAYLOAD,
    reason::REQUIREMENTS,
    reason::SCHEME,
    reason::VERSION,
];

/// Held when the facilitator's answer does not describe the payment it was asked about.
pub const SETTLE_MISMATCH: &str = "settle_answer_mismatch";
/// A retry of a payment that settled and was delivered.
pub const ALREADY_SETTLED: &str = "payment_already_settled";

#[expect(
    clippy::too_many_lines,
    reason = "the payment flow reads top to bottom, one step per check"
)]
async fn handle<S>(shared: Arc<Shared>, req: Request<Body>, inner: S) -> Response
where
    S: Service<Request<Body>, Response = Response, Error = Infallible> + Send + 'static,
    S::Future: Send + 'static,
{
    let s = &shared;
    let Some(raw) = req.headers().get(PAYMENT_SIGNATURE_HEADER) else {
        return s.required(
            StatusCode::PAYMENT_REQUIRED,
            "PAYMENT-SIGNATURE header is required",
            None,
        );
    };
    let payment: PaymentPayload = match decode_header(raw.as_bytes()) {
        Ok(p) => p,
        Err(HeaderError::TooLong) => {
            return s.required(
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                reason::PAYLOAD,
                None,
            );
        }
        Err(_) => return s.required(StatusCode::BAD_REQUEST, reason::PAYLOAD, None),
    };
    let Some(offer) = s.offer_for(&payment.accepted) else {
        return s.required(StatusCode::PAYMENT_REQUIRED, reason::REQUIREMENTS, None);
    };
    let now = s.clock.now();
    let checked = match exact::check(&payment, &offer, now) {
        Ok(v) => v,
        Err(r) => return s.required(StatusCode::PAYMENT_REQUIRED, r, None),
    };
    let latest = now
        .saturating_add(offer.max_timeout_seconds)
        .saturating_add(VALIDITY_SKEW_SECS);
    if checked.valid_before > U256::from(latest) {
        return s.required(StatusCode::PAYMENT_REQUIRED, reason::VALID_BEFORE, None);
    }
    // The permit and the client slot move into the task below and end with it, not with
    // this future: a client that hangs up cannot free them while work is under way.
    let Ok(permit) = Arc::clone(&s.gate).try_acquire_owned() else {
        return busy(); // LIMIT(in-flight)
    };
    let client = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| client_key(*peer, req.headers(), &s.config.trusted_proxies));
    let Some(slot) = Clients::enter(&s.clients, client) else {
        return busy(); // LIMIT(per-client)
    };
    let Ok(asset) = Address::from_str(&offer.asset) else {
        return s.required(StatusCode::PAYMENT_REQUIRED, reason::REQUIREMENTS, None);
    };
    let key = NonceKey {
        asset,
        payer: checked.payer,
        nonce: checked.nonce,
    };
    let expires_at = u64::try_from(checked.valid_before).map_or(latest, |v| v.min(latest));
    let redeliver = match s.store.claim(key, expires_at).await {
        Claim::Fresh => None,
        Claim::Full => return busy(),
        Claim::Taken(ClaimState::Settled {
            delivery: Delivery::NotDelivered,
            ..
        }) => match s.store.redeliver(key).await {
            Some(tx) => Some(tx),
            None => return error(StatusCode::CONFLICT, "payment_in_flight"),
        },
        Claim::Taken(ClaimState::Settled {
            transaction,
            delivery: Delivery::Delivered,
        }) => {
            let settled = rebuilt(&offer, &checked, transaction);
            return with_payment_response(error(StatusCode::CONFLICT, ALREADY_SETTLED), &settled);
        }
        Claim::Taken(
            ClaimState::Settled {
                delivery: Delivery::Serving,
                ..
            }
            | ClaimState::InFlight,
        ) => {
            return error(StatusCode::CONFLICT, "payment_in_flight");
        }
        Claim::Taken(ClaimState::Held { reason }) => {
            return error(StatusCode::GATEWAY_TIMEOUT, reason);
        }
    };
    if redeliver.is_some() {
        tracing::info!(payer = %checked.payer, "x402 settled payment served again");
    } else {
        tracing::info!(payer = %checked.payer, amount = %checked.value, network = %offer.network, "x402 payment claimed");
    }
    let (tx, rx) = oneshot::channel();
    let job = Job {
        guard: ClaimGuard {
            store: Arc::clone(&s.store),
            key,
            phase: if redeliver.is_some() {
                Phase::Settled
            } else {
                Phase::Claimed
            },
        },
        shared: Arc::clone(&shared),
        request: FacilitatorRequest {
            x402_version: X402_VERSION,
            payment_payload: payment,
            payment_requirements: offer,
        },
        checked,
        req,
        reply: tx,
        _permit: permit,
        _slot: slot,
    };
    tokio::spawn(job.run(inner, redeliver));
    // If the task died, its guard already recorded the claim's fate.
    rx.await
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, reason::SETTLE_ERROR))
}

/// The settlement a retry of a settled payment is answered with.
fn rebuilt(
    offer: &PaymentRequirements,
    v: &exact::Verified,
    transaction: String,
) -> SettleResponse {
    SettleResponse {
        success: true,
        error_reason: None,
        error_message: None,
        payer: Some(v.payer.to_checksum(None)),
        transaction,
        network: offer.network.clone(),
        amount: Some(v.value.to_string()),
        extensions: None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Claimed, outcome not yet known.
    Claimed,
    /// Settled, not yet delivered.
    Settled,
    /// The claim's fate is recorded.
    Done,
}

/// Records a claim's fate if its task ends without doing so (a panic in the handler or a
/// facilitator client): an unknown outcome holds, a settled payment is not delivered.
struct ClaimGuard {
    store: Arc<dyn NonceStore>,
    key: NonceKey,
    phase: Phase,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        let (store, key, phase) = (Arc::clone(&self.store), self.key, self.phase);
        if phase == Phase::Done {
            return;
        }
        tracing::warn!(payer = %key.payer, "x402 payment task ended early: claim recorded");
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                if phase == Phase::Claimed {
                    store.hold(key, reason::SETTLE_ERROR).await;
                } else {
                    store.delivered(key, false).await;
                }
            });
        }
    }
}

/// One payment past its claim: verify, settle, run the handler, record the outcome. It
/// runs in its own task holding the permit and the client slot, so it always ends the
/// claim whether or not the client is still there.
struct Job {
    guard: ClaimGuard,
    shared: Arc<Shared>,
    request: FacilitatorRequest,
    checked: exact::Verified,
    req: Request<Body>,
    reply: oneshot::Sender<Response>,
    _permit: OwnedSemaphorePermit,
    _slot: ClientSlot,
}

impl Job {
    fn end(mut self, resp: Response) {
        self.guard.phase = Phase::Done;
        let _ = self.reply.send(resp);
    }

    async fn run<S>(mut self, inner: S, redeliver: Option<String>)
    where
        S: Service<Request<Body>, Response = Response, Error = Infallible> + Send + 'static,
        S::Future: Send + 'static,
    {
        let shared = Arc::clone(&self.shared);
        let (store, key) = (Arc::clone(&shared.store), self.guard.key);
        let settled = if let Some(tx) = redeliver {
            Some(rebuilt(
                &self.request.payment_requirements,
                &self.checked,
                tx,
            ))
        } else {
            if let Err(o) = verify(&shared, key, &self.request).await {
                let resp = shared.respond(o);
                return self.end(resp);
            }
            if shared.config.mode == SettleMode::SettleFirst {
                match settle(&shared, key, &self.request, &self.checked).await {
                    Ok(s) => {
                        self.guard.phase = Phase::Settled;
                        Some(s)
                    }
                    Err(o) => {
                        let resp = shared.respond(o);
                        return self.end(resp);
                    }
                }
            } else {
                None
            }
        };
        if let Some(s) = settled {
            if self.reply.is_closed() {
                // Paid, nobody to serve: a retry of the same proof is served then.
                store.delivered(key, false).await;
                self.guard.phase = Phase::Done;
                return;
            }
            let resp = self.serve(inner, Some(s.transaction.clone())).await;
            return self
                .deliver(resp.map(|r| with_payment_response(r, &s)))
                .await;
        }
        // Settle-after: nothing has moved yet.
        if self.reply.is_closed() {
            store.release(key).await;
            self.guard.phase = Phase::Done;
            return;
        }
        let resp = match self.serve(inner, None).await {
            Some(r) if r.status().is_success() => r,
            other => {
                store.release(key).await;
                let resp =
                    other.unwrap_or_else(|| error(StatusCode::GATEWAY_TIMEOUT, "handler_timeout"));
                return self.end(resp);
            }
        };
        match settle(&shared, key, &self.request, &self.checked).await {
            Ok(s) => {
                self.guard.phase = Phase::Settled;
                self.deliver(Some(with_payment_response(resp, &s))).await;
            }
            Err(o) => {
                let resp = shared.respond(o);
                self.end(resp);
            }
        }
    }

    /// The wrapped handler, within its deadline.
    async fn serve<S>(&mut self, inner: S, transaction: Option<String>) -> Option<Response>
    where
        S: Service<Request<Body>, Response = Response, Error = Infallible> + Send + 'static,
        S::Future: Send + 'static,
    {
        let mut req = std::mem::take(&mut self.req);
        req.extensions_mut().insert(Paid {
            payer: self.checked.payer,
            amount: self.checked.value,
            transaction,
        });
        tokio::time::timeout(self.shared.config.handler_timeout, run(inner, req))
            .await
            .ok()
    }

    /// Hands a paid response back; delivered only if it is a success and the client took it.
    async fn deliver(mut self, resp: Option<Response>) {
        let resp = resp.unwrap_or_else(|| error(StatusCode::GATEWAY_TIMEOUT, "handler_timeout"));
        let ok = resp.status().is_success();
        let (store, key) = (Arc::clone(&self.shared.store), self.guard.key);
        self.guard.phase = Phase::Done;
        let (reply, _) = oneshot::channel();
        let sent = std::mem::replace(&mut self.reply, reply).send(resp).is_ok();
        store.delivered(key, ok && sent).await;
        if !(ok && sent) {
            tracing::warn!(payer = %key.payer, "x402 paid but not delivered: a retry is served");
        }
    }
}

async fn run<S>(inner: S, req: Request<Body>) -> Response
where
    S: Service<Request<Body>, Response = Response, Error = Infallible>,
{
    match inner.oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    }
}

/// Verify moves no money, so every way it fails releases the claim.
async fn verify(
    shared: &Shared,
    key: NonceKey,
    request: &FacilitatorRequest,
) -> Result<(), Outcome> {
    let verdict = tokio::time::timeout(
        shared.config.verify_timeout,
        shared.facilitator.verify(request.clone()),
    )
    .await;
    let failed = match verdict {
        Ok(Ok(v)) if v.is_valid => {
            if v.payer
                .as_deref()
                .is_none_or(|p| same_address(p, key.payer))
            {
                return Ok(());
            }
            tracing::warn!(payer = %key.payer, "x402 verify answer names another payer");
            Outcome::Unavailable(StatusCode::BAD_GATEWAY)
        }
        Ok(Ok(v)) => Outcome::Refused {
            reason: v
                .invalid_reason
                .unwrap_or_else(|| reason::VERIFY_ERROR.into()),
            settle: None,
        },
        Ok(Err(FacilitatorError::Timeout)) | Err(_) => {
            Outcome::Unavailable(StatusCode::GATEWAY_TIMEOUT)
        }
        Ok(Err(_)) => Outcome::Unavailable(StatusCode::BAD_GATEWAY),
    };
    shared.store.release(key).await;
    Err(failed)
}

fn same_address(text: &str, want: Address) -> bool {
    Address::from_str(text).is_ok_and(|a| a == want)
}

/// `0x` and 32 bytes of hex: what a transaction hash looks like.
fn is_tx_hash(t: &str) -> bool {
    t.strip_prefix("0x")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Settles, recording the outcome; `Ok` only for a success that describes this payment.
async fn settle(
    shared: &Shared,
    key: NonceKey,
    request: &FacilitatorRequest,
    checked: &exact::Verified,
) -> Result<SettleResponse, Outcome> {
    let answer = tokio::time::timeout(
        shared.config.settle_timeout,
        shared.facilitator.settle(request.clone()),
    )
    .await;
    let store = &shared.store;
    let held = |status, why: &'static str| async move {
        store.hold(key, why).await;
        tracing::warn!(payer = %key.payer, reason = why, "x402 settlement outcome unknown: claim held");
        Err(Outcome::Held(status, why))
    };
    match answer {
        Ok(Ok(s)) if s.success => {
            let matches = s.network == request.payment_requirements.network
                && is_tx_hash(&s.transaction)
                && s.payer
                    .as_deref()
                    .is_none_or(|p| same_address(p, key.payer))
                && s.amount
                    .as_deref()
                    .is_none_or(|a| a == checked.value.to_string());
            if !matches {
                return held(StatusCode::BAD_GATEWAY, SETTLE_MISMATCH).await;
            }
            store.settled(key, s.transaction.clone()).await;
            tracing::info!(payer = %key.payer, "x402 payment settled");
            Ok(s)
        }
        Ok(Ok(s)) => {
            let why = s.error_reason.as_deref().unwrap_or(reason::SETTLE_ERROR);
            if !s.transaction.is_empty() {
                return held(StatusCode::BAD_GATEWAY, reason::TRANSACTION_STATE).await;
            }
            if let Some(&why) = RELEASE_ON_SETTLE.iter().find(|r| **r == why) {
                store.release(key).await;
                return Err(Outcome::Refused {
                    reason: why.to_owned(),
                    settle: Some(Box::new(s)),
                });
            }
            let kept = match why {
                reason::SETTLEMENT_PENDING => reason::SETTLEMENT_PENDING,
                reason::NONCE_REUSED => reason::NONCE_REUSED,
                _ => reason::SETTLE_ERROR,
            };
            held(StatusCode::GATEWAY_TIMEOUT, kept).await
        }
        Ok(Err(FacilitatorError::Timeout)) => {
            held(StatusCode::GATEWAY_TIMEOUT, reason::SETTLE_ERROR).await
        }
        Ok(Err(_)) => held(StatusCode::BAD_GATEWAY, reason::SETTLE_ERROR).await,
        Err(_) => held(StatusCode::GATEWAY_TIMEOUT, "settle_timeout").await,
    }
}

/// Who holds a payment slot: an IPv4 address or an IPv6 /64 (as `xpay_http::client`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ClientKey {
    V4(Ipv4Addr),
    V6Net([u16; 4]),
}

fn key_of(ip: IpAddr) -> ClientKey {
    match ip {
        IpAddr::V4(v4) => ClientKey::V4(v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or_else(
            || {
                let s = v6.segments();
                ClientKey::V6Net([s[0], s[1], s[2], s[3]])
            },
            ClientKey::V4,
        ),
    }
}

/// The TCP peer, or — only from a trusted proxy — the right-most `X-Forwarded-For` hop.
fn client_key(peer: SocketAddr, headers: &HeaderMap, trusted: &[IpAddr]) -> ClientKey {
    if trusted.contains(&peer.ip())
        && let Some(line) = headers.get_all("x-forwarded-for").iter().next_back()
        && let Some(hop) = line.to_str().ok().and_then(|l| l.rsplit(',').next())
        && let Ok(ip) = hop.trim().parse::<IpAddr>()
    {
        return key_of(ip);
    }
    key_of(peer.ip())
}

/// Payment slots per client. Bounded by `max_in_flight`: a slot is only taken with a permit.
struct Clients {
    per: usize,
    held: Mutex<HashMap<ClientKey, usize>>,
}

impl Clients {
    fn enter(this: &Arc<Self>, client: Option<ClientKey>) -> Option<ClientSlot> {
        let Some(key) = client else {
            return Some(ClientSlot(None));
        };
        let mut held = this.held.lock().unwrap_or_else(PoisonError::into_inner);
        let n = held.entry(key).or_insert(0);
        if *n >= this.per {
            return None;
        }
        *n += 1;
        Some(ClientSlot(Some((Arc::clone(this), key))))
    }
}

struct ClientSlot(Option<(Arc<Clients>, ClientKey)>);

impl Drop for ClientSlot {
    fn drop(&mut self) {
        if let Some((clients, key)) = self.0.take() {
            let mut held = clients.held.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(n) = held.get_mut(&key) {
                *n -= 1;
                if *n == 0 {
                    held.remove(&key);
                }
            }
        }
    }
}

impl Shared {
    /// The offer the payment claims to accept, matched on every field the buyer signed for.
    /// The seller's own offer is what is checked and settled, never the buyer's copy.
    fn offer_for(&self, accepted: &PaymentRequirements) -> Option<PaymentRequirements> {
        self.config
            .accepts
            .iter()
            .find(|o| {
                o.scheme == accepted.scheme
                    && o.network == accepted.network
                    && o.amount == accepted.amount
                    && o.asset.eq_ignore_ascii_case(&accepted.asset)
                    && o.pay_to.eq_ignore_ascii_case(&accepted.pay_to)
            })
            .cloned()
    }

    fn required(
        &self,
        status: StatusCode,
        error: &str,
        settle: Option<&SettleResponse>,
    ) -> Response {
        let body = PaymentRequired {
            x402_version: X402_VERSION,
            error: Some(error.to_owned()),
            resource: self.config.resource.clone(),
            accepts: self.config.accepts.clone(),
            extensions: None,
        };
        let mut resp = (status, axum::Json(&body)).into_response();
        if let Some(v) = encode_header(&body)
            .ok()
            .and_then(|h| HeaderValue::from_str(&h).ok())
        {
            resp.headers_mut().insert(PAYMENT_REQUIRED_HEADER, v);
        }
        if let Some(settle) = settle {
            resp = with_payment_response(resp, settle);
        }
        resp
    }

    fn respond(&self, outcome: Outcome) -> Response {
        match outcome {
            Outcome::Refused { reason, settle } => {
                self.required(StatusCode::PAYMENT_REQUIRED, &reason, settle.as_deref())
            }
            Outcome::Unavailable(status) => error(status, "facilitator_unavailable"),
            Outcome::Held(status, reason) => error(status, reason),
        }
    }
}

fn with_payment_response(mut resp: Response, settle: &SettleResponse) -> Response {
    if let Some(v) = encode_header(settle)
        .ok()
        .and_then(|h| HeaderValue::from_str(&h).ok())
    {
        resp.headers_mut().insert(PAYMENT_RESPONSE_HEADER, v);
    }
    resp
}

fn error(status: StatusCode, error: &str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": error }))).into_response()
}

fn busy() -> Response {
    let mut r = error(StatusCode::SERVICE_UNAVAILABLE, "payments_busy");
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    r
}
