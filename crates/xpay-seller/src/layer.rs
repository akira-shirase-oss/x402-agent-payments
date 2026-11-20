//! The payment layer. See the crate docs for the flow; `docs/DECISIONS.md` D-005..D-008 for
//! why it is shaped this way.

use std::convert::Infallible;
use std::str::FromStr as _;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use alloy_primitives::{Address, U256};
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::sync::Semaphore;
use tower::{Layer, Service, ServiceExt as _};
use xpay_x402::clock::Clock;
use xpay_x402::exact;
use xpay_x402::facilitator::{BoxFuture, Facilitator, FacilitatorError};
use xpay_x402::{
    FacilitatorRequest, HeaderError, PAYMENT_REQUIRED_HEADER, PAYMENT_RESPONSE_HEADER,
    PAYMENT_SIGNATURE_HEADER, PaymentPayload, PaymentRequired, PaymentRequirements, ResourceInfo,
    SettleResponse, X402_VERSION, decode_header, encode_header, reason,
};

use crate::store::{Claim, ClaimState, NonceKey, NonceStore};

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
    /// Bound: payments past the offline checks at once; more get 503.
    pub max_in_flight: usize,
}

impl PaymentConfig {
    pub fn new(resource: ResourceInfo, accepts: Vec<PaymentRequirements>) -> Self {
        Self {
            resource,
            accepts,
            mode: SettleMode::SettleFirst,
            verify_timeout: Duration::from_secs(5),
            settle_timeout: Duration::from_secs(20),
            max_in_flight: 256,
        }
    }
}

/// What the handler learns about the payment: `Extension<Paid>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paid {
    pub payer: Address,
    pub amount: U256,
    /// The settlement transaction (settle-first only; `None` before settlement).
    pub transaction: Option<String>,
}

struct Shared {
    config: PaymentConfig,
    facilitator: Arc<dyn Facilitator>,
    store: Arc<dyn NonceStore>,
    clock: Arc<dyn Clock>,
    gate: Arc<Semaphore>,
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

impl PaymentLayer {
    pub fn new(
        config: PaymentConfig,
        facilitator: Arc<dyn Facilitator>,
        store: Arc<dyn NonceStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let gate = Arc::new(Semaphore::new(config.max_in_flight));
        Self {
            shared: Arc::new(Shared {
                config,
                facilitator,
                store,
                clock,
                gate,
            }),
        }
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

/// The outcome of the facilitator half, decided in a spawned task so a client that hangs
/// up mid-settle cannot cancel the bookkeeping.
enum Outcome {
    Settled(SettleResponse),
    /// Verified, not settled (settle-after mode).
    Verified,
    /// Definitely not paid: the claim was released.
    Refused {
        reason: String,
        settle: Option<SettleResponse>,
    },
    /// Facilitator unreachable before anything could move: the claim was released.
    Unavailable(StatusCode),
    /// Settlement outcome unknown: the claim is held.
    Held(StatusCode, &'static str),
}

#[expect(
    clippy::too_many_lines,
    reason = "the payment flow reads top to bottom, one step per check"
)]
async fn handle<S>(shared: Arc<Shared>, mut req: Request<Body>, inner: S) -> Response
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
    let Ok(_permit) = Arc::clone(&s.gate).try_acquire_owned() else {
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
    match s.store.claim(key, latest).await {
        Claim::Fresh => {}
        Claim::Full => return busy(),
        Claim::Taken(ClaimState::Settled { .. }) => {
            return s.required(StatusCode::PAYMENT_REQUIRED, reason::NONCE_REUSED, None);
        }
        Claim::Taken(ClaimState::InFlight) => {
            return error(StatusCode::CONFLICT, "payment_in_flight");
        }
        Claim::Taken(ClaimState::Held { reason }) => {
            return error(StatusCode::GATEWAY_TIMEOUT, reason);
        }
    }
    tracing::info!(payer = %checked.payer, amount = %checked.value, network = %offer.network, "x402 payment claimed");
    let request = FacilitatorRequest {
        x402_version: X402_VERSION,
        payment_payload: payment,
        payment_requirements: offer,
    };
    let settle_now = s.config.mode == SettleMode::SettleFirst;
    let task = tokio::spawn(facilitate(
        Arc::clone(&shared),
        key,
        request.clone(),
        settle_now,
    ));
    let Ok(outcome) = task.await else {
        // The task panicked after the claim: the outcome is unknown, so hold.
        shared.store.hold(key, reason::SETTLE_ERROR).await;
        return error(StatusCode::INTERNAL_SERVER_ERROR, reason::SETTLE_ERROR);
    };
    let s = &shared;
    match outcome {
        Outcome::Settled(settle) => {
            req.extensions_mut().insert(Paid {
                payer: checked.payer,
                amount: checked.value,
                transaction: Some(settle.transaction.clone()),
            });
            let resp = run(inner, req).await;
            with_payment_response(resp, &settle)
        }
        Outcome::Verified => {
            req.extensions_mut().insert(Paid {
                payer: checked.payer,
                amount: checked.value,
                transaction: None,
            });
            let resp = run(inner, req).await;
            if !resp.status().is_success() {
                s.store.release(key).await;
                return resp;
            }
            match tokio::spawn(settle(Arc::clone(&shared), key, request)).await {
                Ok(Outcome::Settled(settle)) => with_payment_response(resp, &settle),
                Ok(other) => s.respond(other),
                Err(_) => {
                    shared.store.hold(key, reason::SETTLE_ERROR).await;
                    error(StatusCode::INTERNAL_SERVER_ERROR, reason::SETTLE_ERROR)
                }
            }
        }
        other => s.respond(other),
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

async fn facilitate(
    shared: Arc<Shared>,
    key: NonceKey,
    request: FacilitatorRequest,
    settle_now: bool,
) -> Outcome {
    let verdict = tokio::time::timeout(
        shared.config.verify_timeout,
        shared.facilitator.verify(request.clone()),
    )
    .await;
    let unavailable = match verdict {
        Ok(Ok(v)) if v.is_valid => None,
        Ok(Ok(v)) => {
            shared.store.release(key).await;
            let reason = v
                .invalid_reason
                .unwrap_or_else(|| reason::VERIFY_ERROR.into());
            return Outcome::Refused {
                reason,
                settle: None,
            };
        }
        Ok(Err(FacilitatorError::Timeout)) | Err(_) => Some(StatusCode::GATEWAY_TIMEOUT),
        Ok(Err(_)) => Some(StatusCode::BAD_GATEWAY),
    };
    if let Some(status) = unavailable {
        // Verify moves no money: releasing lets the buyer retry the same authorisation.
        shared.store.release(key).await;
        return Outcome::Unavailable(status);
    }
    if settle_now {
        settle(shared, key, request).await
    } else {
        Outcome::Verified
    }
}

async fn settle(shared: Arc<Shared>, key: NonceKey, request: FacilitatorRequest) -> Outcome {
    let answer = tokio::time::timeout(
        shared.config.settle_timeout,
        shared.facilitator.settle(request),
    )
    .await;
    let store = &shared.store;
    match answer {
        Ok(Ok(s)) if s.success => {
            store.settled(key, s.transaction.clone()).await;
            tracing::info!(payer = %key.payer, "x402 payment settled");
            Outcome::Settled(s)
        }
        Ok(Ok(s)) if s.error_reason.as_deref() == Some(reason::SETTLEMENT_PENDING) => {
            store.hold(key, reason::SETTLEMENT_PENDING).await;
            tracing::warn!(payer = %key.payer, "x402 settlement pending: claim held");
            Outcome::Held(StatusCode::GATEWAY_TIMEOUT, reason::SETTLEMENT_PENDING)
        }
        Ok(Ok(s)) => {
            store.release(key).await;
            let reason = s
                .error_reason
                .clone()
                .unwrap_or_else(|| reason::SETTLE_ERROR.into());
            Outcome::Refused {
                reason,
                settle: Some(s),
            }
        }
        Ok(Err(e)) => {
            store.hold(key, reason::SETTLE_ERROR).await;
            tracing::warn!(payer = %key.payer, error = %e, "x402 settle answer lost: claim held");
            let status = if e == FacilitatorError::Timeout {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            };
            Outcome::Held(status, reason::SETTLE_ERROR)
        }
        Err(_) => {
            store.hold(key, "settle_timeout").await;
            tracing::warn!(payer = %key.payer, "x402 settle deadline passed: claim held");
            Outcome::Held(StatusCode::GATEWAY_TIMEOUT, "settle_timeout")
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
                self.required(StatusCode::PAYMENT_REQUIRED, &reason, settle.as_ref())
            }
            Outcome::Unavailable(status) => error(status, "facilitator_unavailable"),
            Outcome::Held(status, reason) => error(status, reason),
            Outcome::Settled(_) | Outcome::Verified => {
                error(StatusCode::INTERNAL_SERVER_ERROR, reason::SETTLE_ERROR)
            }
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
