//! The health contract (Backend Platform's `a_dead_control_database_is_reported_honestly`
//! pattern):
//!
//! - `GET /livez`: the process answers. Never touches a dependency and is never shed, so
//!   a supervisor does not restart a busy but healthy server.
//! - `GET /readyz`: should this server get traffic? The database is probed with a deadline;
//!   unreachable or slow is reported as such (503), never as green. One probe runs at a time
//!   and its answer is reused for a second, so a flood of probes cannot crowd the pool.
//! - `GET /errorz`: 503 when server errors over the last five minutes exceed the threshold,
//!   so Service Monitor can alert on an error rate from outside, as a customer would.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Mutex;
use tokio::time::Instant;
use xpay_http::stack::ErrorWindow;

/// How long one probe's answer is reused.
pub const REUSE: Duration = Duration::from_secs(1);

/// What readiness needs.
#[derive(Debug, Clone)]
pub struct Health {
    db: PgPool,
    deadline: Duration,
    last: Arc<Mutex<Option<(Instant, Probe)>>>,
    errors: Arc<ErrorWindow>,
    permille: u64,
    min_requests: u64,
}

/// One probe's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// The database answered within the deadline.
    Ok,
    /// It answered with an error, or refused the connection.
    Unreachable,
    /// It did not answer within the deadline.
    Timeout,
}

impl Health {
    /// Readiness over `db` with a probe `deadline`, and the error window's threshold.
    pub fn new(
        db: PgPool,
        deadline: Duration,
        errors: Arc<ErrorWindow>,
        permille: u64,
        min_requests: u64,
    ) -> Self {
        Self {
            db,
            deadline,
            last: Arc::new(Mutex::new(None)),
            errors,
            permille,
            min_requests,
        }
    }

    async fn probe(&self) -> Probe {
        let mut last = self.last.lock().await;
        if let Some((at, probe)) = *last
            && at.elapsed() < REUSE
        {
            return probe;
        }
        let deadline = self.deadline; // LIMIT(ready-deadline)
        let probe =
            match tokio::time::timeout(deadline, sqlx::query("SELECT 1").execute(&self.db)).await {
                Ok(Ok(_)) => Probe::Ok,
                Ok(Err(_)) => Probe::Unreachable,
                Err(_) => Probe::Timeout,
            };
        *last = Some((Instant::now(), probe));
        probe
    }
}

/// `GET /livez`.
pub async fn livez() -> Json<serde_json::Value> {
    Json(json!({ "status": "live" }))
}

/// `GET /readyz`.
pub async fn readyz(State(health): State<Health>) -> Response {
    match health.probe().await {
        Probe::Ok => Json(json!({ "status": "ready", "database": "ok" })).into_response(),
        failed => {
            let database = if failed == Probe::Timeout {
                "timeout"
            } else {
                "unreachable"
            };
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "status": "unready",
                    "database": database,
                    "error": { "code": "unready", "message": "the database is not answering" },
                })),
            )
                .into_response()
        }
    }
}

/// `GET /errorz`.
pub async fn errorz(State(health): State<Health>) -> Response {
    let counts = health.errors.counts();
    let alerting = counts.requests >= health.min_requests
        && counts.server_errors.saturating_mul(1_000)
            > health.permille.saturating_mul(counts.requests);
    let body = json!({
        "window_seconds": ErrorWindow::SLOT.as_secs() * ErrorWindow::SLOTS as u64,
        "requests": counts.requests,
        "server_errors": counts.server_errors,
        "threshold_permille": health.permille,
    });
    if alerting {
        let mut body = body;
        body["error"] =
            json!({ "code": "error_rate", "message": "server errors are above the threshold" });
        (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
    } else {
        Json(body).into_response()
    }
}
