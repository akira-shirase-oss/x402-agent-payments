//! The Tower stack. `ServiceBuilder` lists layers outermost first: a request travels down
//! the list, its response back up (rust-mastery chapter 12).
//!
//! Also here: load shedding ([`shed`]) and the server-error window ([`ErrorWindow`]) that
//! `/errorz` reports, so Service Monitor can alert on an error rate from outside.

use std::any::Any;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self as axum_mw, Next};
use axum::response::{IntoResponse, Response};
use tokio::sync::Semaphore;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{
    MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnFailure, DefaultOnResponse, TraceLayer};
use tracing::{Level, Span};

/// Request id header, both directions.
pub const REQUEST_ID_HEADER: &str = "x-request-id";
/// Longest client request id kept.
pub const MAX_REQUEST_ID_LEN: usize = 64;
/// Largest error body the envelope layer re-reads to add the request id.
const MAX_ERROR_BODY: usize = 8 * 1024;
/// Paths that are never counted in the error window: a probe's own 503 must not feed the
/// rate it reports.
pub const HEALTH_PATHS: [&str; 3] = ["/livez", "/readyz", "/errorz"];

/// The stack's bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackLimits {
    /// Deadline for a whole request, body included: past it, 503 `timeout`.
    pub request_timeout: Duration,
    /// Largest request body: past it, 413 `payload_too_large`.
    pub max_body_bytes: usize,
}

impl Default for StackLimits {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(5),
            max_body_bytes: 64 * 1024,
        }
    }
}

/// One error body for every 4xx and 5xx: `{"error": {"code", "message", "request_id"}}`.
pub fn error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    let body = serde_json::json!({ "error": { "code": code, "message": message.into() } });
    (status, axum::Json(body)).into_response()
}

/// UUIDv7 request ids.
#[derive(Debug, Clone, Copy, Default)]
pub struct MakeRequestUuidV7;

impl MakeRequestId for MakeRequestUuidV7 {
    fn make_request_id<B>(&mut self, _request: &Request<B>) -> Option<RequestId> {
        HeaderValue::from_str(&uuid::Uuid::now_v7().to_string())
            .ok()
            .map(RequestId::new)
    }
}

/// True for client ids safe to copy into logs: 1–64 of `[A-Za-z0-9._-]`.
pub fn is_acceptable_request_id(value: &str) -> bool {
    (1..=MAX_REQUEST_ID_LEN).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

async fn sanitize_request_id(mut request: Request) -> Request {
    let acceptable = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .map(|value| value.to_str().is_ok_and(is_acceptable_request_id));
    if acceptable == Some(false) {
        request.headers_mut().remove(REQUEST_ID_HEADER);
    }
    request
}

/// Root span: method, matched route (low cardinality) and request id. Never the query
/// string, never a header value: query strings can carry tokens.
fn make_span(request: &Request<Body>) -> Span {
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", MatchedPath::as_str);
    tracing::info_span!("request", method = %request.method(), route, request_id = %request_id)
}

fn code_for(status: StatusCode) -> &'static str {
    match status {
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
        StatusCode::REQUEST_TIMEOUT => "body_timeout",
        StatusCode::SERVICE_UNAVAILABLE => "timeout",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
        StatusCode::UNPROCESSABLE_ENTITY | StatusCode::BAD_REQUEST => "invalid_request",
        s if s.is_client_error() => "invalid_request",
        _ => "internal",
    }
}

/// Every 4xx/5xx leaves as the envelope, carrying the request id.
async fn error_envelope(request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let response = next.run(request).await;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"application/json"));
    let (mut parts, body) = response.into_parts();
    let mut value = if is_json {
        axum::body::to_bytes(body, MAX_ERROR_BODY)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    } else {
        None
    }
    .filter(|v| v.get("error").is_some_and(serde_json::Value::is_object))
    .unwrap_or_else(|| {
        // A framework rejection's text can echo client input: use the status's own words.
        let message = status.canonical_reason().unwrap_or("error");
        serde_json::json!({ "error": { "code": code_for(status), "message": message } })
    });
    if let (Some(id), Some(error)) = (
        request_id,
        value.get_mut("error").and_then(|e| e.as_object_mut()),
    ) {
        error.insert("request_id".to_owned(), serde_json::Value::String(id));
    }
    let bytes = serde_json::to_vec(&value).unwrap_or_default();
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(bytes))
}

/// A caught panic becomes a 500 envelope; the payload is logged, never returned.
fn panic_response(payload: Box<dyn Any + Send + 'static>) -> Response {
    let message = match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => payload.downcast::<&'static str>().map_or_else(
            |_| "non-string panic payload".to_owned(),
            |m| (*m).to_owned(),
        ),
    };
    tracing::error!(panic = %message, "handler panicked");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "internal error",
    )
}

/// The in-flight gate: at most this many requests run at once behind [`shed`].
pub fn gate(max_in_flight: usize) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(max_in_flight.max(1))) // LIMIT(in-flight)
}

/// Rejects at once with 503 `overloaded` when the gate is full, instead of queueing until
/// every request times out. Mount with `route_layer(from_fn_with_state(gate, shed))`.
pub async fn shed(State(gate): State<Arc<Semaphore>>, request: Request, next: Next) -> Response {
    let Ok(permit) = gate.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded",
            "the server is at capacity; retry shortly",
        );
    };
    let response = next.run(request).await;
    drop(permit);
    response
}

/// Server errors over a sliding window of [`ErrorWindow::SLOTS`] × [`ErrorWindow::SLOT`].
#[derive(Debug)]
pub struct ErrorWindow {
    started: tokio::time::Instant,
    slots: Mutex<[Slot; ErrorWindow::SLOTS]>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    epoch: u64,
    requests: u64,
    server_errors: u64,
}

/// Counts in the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorCounts {
    /// Requests answered (health probes excluded).
    pub requests: u64,
    /// Of those, answered 5xx (shed and timed-out requests included).
    pub server_errors: u64,
}

impl Default for ErrorWindow {
    fn default() -> Self {
        Self {
            started: tokio::time::Instant::now(),
            slots: Mutex::new([Slot::default(); Self::SLOTS]),
        }
    }
}

impl ErrorWindow {
    /// Slots in the window.
    pub const SLOTS: usize = 30;
    /// One slot's span: the window is five minutes.
    pub const SLOT: Duration = Duration::from_secs(10);

    fn epoch(&self) -> u64 {
        self.started.elapsed().as_secs() / Self::SLOT.as_secs()
    }

    /// Records one answer.
    pub fn record(&self, status: StatusCode) {
        let epoch = self.epoch();
        let index = usize::try_from(epoch % Self::SLOTS as u64).unwrap_or(0);
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = &mut slots[index];
        if slot.epoch != epoch {
            *slot = Slot {
                epoch,
                ..Slot::default()
            };
        }
        slot.requests += 1;
        if status.is_server_error() {
            slot.server_errors += 1;
        }
    }

    /// The counts over the window.
    pub fn counts(&self) -> ErrorCounts {
        let epoch = self.epoch();
        let oldest = epoch.saturating_sub(Self::SLOTS as u64 - 1);
        let slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots
            .iter()
            .filter(|s| s.epoch >= oldest && s.epoch <= epoch && s.requests > 0)
            .fold(
                ErrorCounts {
                    requests: 0,
                    server_errors: 0,
                },
                |acc, s| ErrorCounts {
                    requests: acc.requests + s.requests,
                    server_errors: acc.server_errors + s.server_errors,
                },
            )
    }
}

async fn count_errors(
    State(window): State<Arc<ErrorWindow>>,
    request: Request,
    next: Next,
) -> Response {
    let health = HEALTH_PATHS.contains(&request.uri().path());
    let response = next.run(request).await;
    if !health {
        window.record(response.status());
    }
    response
}

/// Applies the global stack to `router`.
pub fn apply(router: Router, limits: StackLimits, window: Arc<ErrorWindow>) -> Router {
    let stack = ServiceBuilder::new()
        // 0. Credentials are redacted from any Debug output of the request.
        .layer(SetSensitiveRequestHeadersLayer::new([
            header::AUTHORIZATION,
            header::COOKIE,
            HeaderName::from_static("idempotency-key"),
        ]))
        // 1. Drop unsafe client ids before anything logs them.
        .layer(axum_mw::map_request(sanitize_request_id))
        // 2. Assign an id when absent. 3. Copy it onto every response.
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuidV7))
        .layer(PropagateRequestIdLayer::x_request_id())
        // 4. Root span, which sees the id; every log line inside it carries it.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(make_span)
                .on_response(DefaultOnResponse::new().level(Level::DEBUG))
                // Each error is logged once, where it is handled; an ERROR line per shed
                // request would turn overload into a log storm.
                .on_failure(DefaultOnFailure::new().level(Level::DEBUG)),
        )
        // 5. Count server errors, shed and timed-out requests included.
        .layer(axum_mw::from_fn_with_state(window, count_errors))
        // 6. One error envelope for everything below.
        .layer(axum_mw::from_fn(error_envelope))
        // 7. Panics become 500 envelopes.
        .layer(CatchPanicLayer::custom(panic_response))
        // 8. Deadline for the whole request, body included: 503, we were slow or the client
        //    stalled; either way the slot comes back.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            limits.request_timeout, // LIMIT(request-timeout)
        ))
        // 9–10. Body limits, by Content-Length up front and while streaming.
        .layer(RequestBodyLimitLayer::new(limits.max_body_bytes)) // LIMIT(body)
        .layer(DefaultBodyLimit::max(limits.max_body_bytes)); // LIMIT(body)
    router.layer(stack)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_policy() {
        for (input, expected) in [
            ("018f2f0e-3c1a-7cc0-9e1e-3f1f6b7f9a01", true),
            ("trace.42_a", true),
            ("", false),
            ("has space", false),
            ("new\nline", false),
            (&*"a".repeat(65), false),
        ] {
            assert_eq!(is_acceptable_request_id(input), expected, "{input:?}");
        }
    }

    #[test]
    fn panic_payloads_become_500s() {
        for payload in [
            Box::new("static") as Box<dyn Any + Send>,
            Box::new(String::from("owned")),
            Box::new(7_u8),
        ] {
            assert_eq!(
                panic_response(payload).status(),
                StatusCode::INTERNAL_SERVER_ERROR
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_error_window_forgets_after_five_minutes() {
        let window = ErrorWindow::default();
        window.record(StatusCode::OK);
        window.record(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            window.counts(),
            ErrorCounts {
                requests: 2,
                server_errors: 1
            }
        );
        tokio::time::advance(Duration::from_secs(150)).await;
        window.record(StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(window.counts().server_errors, 2);
        tokio::time::advance(Duration::from_secs(160)).await;
        // The first two are older than 300 s now; the third is not.
        assert_eq!(
            window.counts(),
            ErrorCounts {
                requests: 1,
                server_errors: 1
            }
        );
        tokio::time::advance(Duration::from_secs(300)).await;
        assert_eq!(window.counts().requests, 0);
    }
}
