//! The payment layer. See the crate docs for the flow; `docs/DECISIONS.md` D-005..D-008 for
//! why it is shaped this way.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use alloy_primitives::{Address, U256, keccak256};
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

use crate::store::{Claim, ClaimState, Delivery, NonceKey, NonceStore, Redelivery, Route};

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
    /// Bound: how many one network (IPv4 /24 or IPv6 /48) may hold, so rotating addresses
    /// inside one allocation does not multiply a client's share.
    pub max_in_flight_per_network: usize,
    /// Proxies (addresses or CIDR ranges) whose `X-Forwarded-For` hops are believed: the
    /// client is the right-most hop that is not itself a trusted proxy.
    pub trusted_proxies: Vec<ProxyNet>,
    /// The layer takes an exclusive lock file here per payee, so a second seller process
    /// for the same `payTo` on this host refuses to start (D-017). On by default
    /// ([`default_lock_dir`]); `None` turns it off.
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
            max_in_flight_per_network: 64,
            trusted_proxies: Vec::new(),
            lock_dir: Some(default_lock_dir()),
        }
    }
}

/// Where payee locks live unless configured: the service's runtime or state directory
/// (systemd), else the user's runtime directory, else a private directory under the
/// temporary directory.
pub fn default_lock_dir() -> PathBuf {
    ["RUNTIME_DIRECTORY", "STATE_DIRECTORY", "XDG_RUNTIME_DIR"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .find(|p| p.is_absolute())
        .unwrap_or_else(|| std::env::temp_dir().join("x402-seller-locks"))
}

/// A trusted proxy: one address or a CIDR range (`"10.0.0.0/8"`, `"2001:db8::/32"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxyNet {
    addr: IpAddr,
    prefix: u8,
}

impl ProxyNet {
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        match (self.addr, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(n) & mask == u32::from(a) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(n) & mask == u128::from(a) & mask
            }
            _ => false,
        }
    }
}

impl From<IpAddr> for ProxyNet {
    fn from(addr: IpAddr) -> Self {
        let addr = canonical(addr);
        let prefix = if addr.is_ipv4() { 32 } else { 128 };
        Self { addr, prefix }
    }
}

/// Why a [`ProxyNet`] does not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an IP address or CIDR range")]
pub struct ProxyNetError;

impl std::str::FromStr for ProxyNet {
    type Err = ProxyNetError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (ip, prefix) = s.split_once('/').map_or((s, None), |(a, p)| (a, Some(p)));
        let net = Self::from(ip.trim().parse::<IpAddr>().map_err(|_| ProxyNetError)?);
        let Some(p) = prefix else {
            return Ok(net);
        };
        let p: u8 = p.trim().parse().map_err(|_| ProxyNetError)?;
        let max = if net.addr.is_ipv4() { 32 } else { 128 };
        if p > max || (p == 0) {
            return Err(ProxyNetError);
        }
        Ok(Self { prefix: p, ..net })
    }
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
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

/// One payee's registration: the store its claims live in, how many layers use it, and
/// the host lock taken once for all of them.
struct Registered {
    store: usize,
    layers: usize,
    _lock: Option<std::fs::File>,
}

/// The process's payees and the store each one's claims live in (D-017): two stores for
/// one payee would each accept the same authorisation once.
static PAYEES: LazyLock<Mutex<HashMap<Payee, Registered>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn payees() -> std::sync::MutexGuard<'static, HashMap<Payee, Registered>> {
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
}

impl Drop for Shared {
    fn drop(&mut self) {
        let mut all = payees();
        for p in &self.payees {
            if let Some(r) = all.get_mut(p) {
                r.layers -= 1;
                if r.layers == 0 {
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
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
    let failed = |e: std::io::Error| ConfigError::Lock(e.kind().to_string());
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(failed)?;
    // A directory others can write to would let them plant the lock file (or a link).
    let meta = std::fs::symlink_metadata(dir).map_err(failed)?;
    // ... or one another user made in a shared temporary directory (`/proc/self` is
    // owned by this process's user).
    let ours = std::fs::metadata("/proc/self").map_or(true, |me| me.uid() == meta.uid());
    if !meta.is_dir() || meta.permissions().mode() & 0o022 != 0 || !ours {
        return Err(ConfigError::Lock("lock directory is not private".into()));
    }
    let name = format!("x402-seller-{}-{:x}.lock", p.0.replace(':', "-"), p.2);
    let path = dir.join(name);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
        return Err(ConfigError::Lock("lock path is not a file".into()));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(failed)?;
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
        if config.max_in_flight == 0
            || config.max_in_flight_per_client == 0
            || config.max_in_flight_per_network == 0
        {
            return Err(ConfigError::ZeroBound);
        }
        let mut wanted: Vec<Payee> = Vec::new();
        for (i, o) in config.accepts.iter().enumerate() {
            let p = payee(o).ok_or(ConfigError::Offer(i))?;
            if !wanted.contains(&p) {
                wanted.push(p);
            }
        }
        let id = Arc::as_ptr(&store).cast::<()>() as usize;
        {
            let mut all = payees();
            if let Some(i) = wanted
                .iter()
                .position(|p| all.get(p).is_some_and(|r| r.store != id))
            {
                return Err(ConfigError::PayToTaken(i));
            }
            // Locks for payees new to this process, all taken before any is registered.
            let mut locks = Vec::new();
            for (i, p) in wanted.iter().enumerate() {
                if all.contains_key(p) {
                    locks.push(None);
                } else if let Some(dir) = &config.lock_dir {
                    locks.push(Some(lock(dir, p, i)?));
                } else {
                    locks.push(None);
                }
            }
            for (p, l) in wanted.iter().zip(locks) {
                all.entry(p.clone())
                    .or_insert(Registered {
                        store: id,
                        layers: 0,
                        _lock: l,
                    })
                    .layers += 1;
            }
        }
        let gate = Arc::new(Semaphore::new(config.max_in_flight)); // LIMIT(payment-in-flight)
        let clients = Arc::new(Clients {
            per: config.max_in_flight_per_client,
            per_network: config.max_in_flight_per_network,
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
/// A retry of a payment that is still being verified, settled or served.
pub const IN_FLIGHT: &str = "payment_in_flight";
/// A retry, on another resource, of a payment that was not delivered.
pub const OTHER_RESOURCE: &str = "payment_for_another_resource";
/// A payment that was not delivered and is out of attempts: for a dispute.
pub const NOT_DELIVERED: &str = "payment_not_delivered";

/// The resource a request is for: its method and path (not the query).
fn route_of(req: &Request<Body>) -> Route {
    let mut buf = Vec::with_capacity(16 + req.uri().path().len());
    buf.extend_from_slice(req.method().as_str().as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(req.uri().path().as_bytes());
    keccak256(buf)
}

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
        return busy();
    };
    // Without the peer address no client can be told from another: refuse rather than
    // run unbounded per client (serve with `into_make_service_with_connect_info`).
    let Some(ConnectInfo(peer)) = req.extensions().get::<ConnectInfo<SocketAddr>>() else {
        tracing::error!("x402 request without ConnectInfo: refused");
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "client_address_unavailable",
        );
    };
    let client = client_ip(*peer, req.headers(), &s.config.trusted_proxies);
    let Some(slot) = Clients::enter(&s.clients, client) else {
        return busy();
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
    let route = route_of(&req);
    let redeliver = match s.store.claim(key, route, expires_at).await {
        Claim::Fresh => None,
        Claim::Full => return busy(),
        Claim::Taken(ClaimState::Settled {
            delivery: Delivery::NotDelivered,
            ..
        }) => match s.store.redeliver(key, route).await {
            Redelivery::Serve(tx) => Some(tx),
            Redelivery::OtherResource => {
                return error(StatusCode::CONFLICT, OTHER_RESOURCE);
            }
            Redelivery::No => return error(StatusCode::CONFLICT, NOT_DELIVERED),
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
            return error(StatusCode::CONFLICT, IN_FLIGHT);
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
    /// Claimed; nothing has been sent to settle, so nothing can have moved.
    Claimed,
    /// A settle call is (or may be) under way: the outcome is unknown.
    Settling,
    /// Settled, not yet delivered.
    Settled,
    /// The claim's fate is recorded.
    Done,
}

/// Records a claim's fate if its task ends without doing so (a panic in the handler or a
/// facilitator client): before settle it is released, during settle it holds, a settled
/// payment is not delivered.
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
                match phase {
                    Phase::Claimed => store.release(key).await,
                    Phase::Settling => store.hold(key, reason::SETTLE_ERROR).await,
                    Phase::Settled | Phase::Done => store.delivered(key, false).await,
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
                self.guard.phase = Phase::Settling;
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
        // Any response the handler finished, short of a server error, is the resource:
        // settle it. A timeout, a 5xx or a panic (the guard) settles nothing.
        let resp = match self.serve(inner, None).await {
            Some(r) if !r.status().is_server_error() => r,
            other => {
                store.release(key).await;
                let resp =
                    other.unwrap_or_else(|| error(StatusCode::GATEWAY_TIMEOUT, "handler_timeout"));
                return self.end(resp);
            }
        };
        self.guard.phase = Phase::Settling;
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

    /// Hands a paid response back. Delivered once the connection took it, whatever its
    /// status (a redirect, a 304 or a 4xx is the resource), unless the handler timed out
    /// or failed with a 5xx: only then is a retry of the same proof served again.
    async fn deliver(mut self, resp: Option<Response>) {
        let ok = resp.as_ref().is_some_and(|r| !r.status().is_server_error());
        let resp = resp.unwrap_or_else(|| error(StatusCode::GATEWAY_TIMEOUT, "handler_timeout"));
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

/// Who holds a payment slot: an IPv4 address or an IPv6 /64 (as `xpay_http::client`), and
/// the network around it: an IPv4 /24 or an IPv6 /48.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ClientKey {
    V4(u32),
    V6(u64),
    Net4(u32),
    Net6(u64),
}

fn keys_of(ip: IpAddr) -> [ClientKey; 2] {
    match canonical(ip) {
        IpAddr::V4(v4) => {
            let a = u32::from(v4);
            [ClientKey::V4(a), ClientKey::Net4(a >> 8)]
        }
        IpAddr::V6(v6) => {
            let a = u128::from(v6);
            let (net64, net48) = ((a >> 64) as u64, (a >> 80) as u64);
            [ClientKey::V6(net64), ClientKey::Net6(net48)]
        }
    }
}

/// Most `X-Forwarded-For` hops walked through trusted proxies.
const MAX_PROXY_HOPS: usize = 8;

/// The TCP peer, or — through trusted proxies — the right-most `X-Forwarded-For` hop that
/// is not a trusted proxy. Hops to its left were written by the client and are never
/// believed; a hop that does not parse stops the walk at the last trusted hop.
fn client_ip(peer: SocketAddr, headers: &HeaderMap, trusted: &[ProxyNet]) -> IpAddr {
    let is_trusted = |ip: IpAddr| trusted.iter().any(|n| n.contains(ip));
    let mut client = canonical(peer.ip());
    if !is_trusted(client) {
        return client;
    }
    let lines: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .map(|v| v.to_str().unwrap_or(""))
        .collect();
    let hops = lines.iter().rev().flat_map(|l| l.rsplit(','));
    for hop in hops.take(MAX_PROXY_HOPS) {
        let Ok(ip) = hop.trim().parse::<IpAddr>() else {
            return client;
        };
        client = canonical(ip);
        if !is_trusted(client) {
            return client;
        }
    }
    client
}

/// Payment slots per client and per network. Bounded by `max_in_flight`: a slot is only
/// taken with a permit.
struct Clients {
    per: usize,
    per_network: usize,
    held: Mutex<HashMap<ClientKey, usize>>,
}

impl Clients {
    fn enter(this: &Arc<Self>, ip: IpAddr) -> Option<ClientSlot> {
        let [client, net] = keys_of(ip);
        let mut held = this.held.lock().unwrap_or_else(PoisonError::into_inner);
        // LIMIT(payment-per-client)
        if held.get(&client).copied().unwrap_or(0) >= this.per {
            return None;
        }
        // LIMIT(payment-per-network)
        if held.get(&net).copied().unwrap_or(0) >= this.per_network {
            return None;
        }
        *held.entry(client).or_insert(0) += 1;
        *held.entry(net).or_insert(0) += 1;
        Some(ClientSlot(Arc::clone(this), [client, net]))
    }
}

struct ClientSlot(Arc<Clients>, [ClientKey; 2]);

impl Drop for ClientSlot {
    fn drop(&mut self) {
        let mut held = self.0.held.lock().unwrap_or_else(PoisonError::into_inner);
        for key in &self.1 {
            if let Some(n) = held.get_mut(key) {
                *n -= 1;
                if *n == 0 {
                    held.remove(key);
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
