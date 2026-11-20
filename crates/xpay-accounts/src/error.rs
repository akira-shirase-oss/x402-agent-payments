//! The kit's errors and their answers. Every message is fixed text: nothing a client sent
//! is echoed, and nothing says whether an account exists where that would help a guesser.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use thiserror::Error;
use xpay_db::DbError;
use xpay_http::stack::error;

/// A refused or failed request.
#[derive(Debug, Error)]
pub enum AccountsError {
    /// A malformed or out-of-bounds value (422).
    #[error("{1}")]
    Invalid(&'static str, &'static str),
    /// The invite code is unknown, used or expired (403).
    #[error("invite invalid")]
    InviteInvalid,
    /// An account with this email exists (409).
    #[error("email taken")]
    EmailTaken,
    /// The link token is unknown, used or expired (400).
    #[error("token invalid")]
    TokenInvalid,
    /// Wrong email or password, with the same work either way (401).
    #[error("sign-in failed")]
    SignInFailed,
    /// The password was right but the email is not verified yet (403).
    #[error("email unverified")]
    EmailUnverified,
    /// A budget is spent (429 with `Retry-After`).
    #[error("rate limited")]
    RateLimited(u64),
    /// No valid session or token (401).
    #[error("unauthenticated")]
    Unauthenticated,
    /// This route needs a signed-in person, not an API token (403).
    #[error("session required")]
    SessionRequired,
    /// The role does not allow it (403).
    #[error("forbidden")]
    Forbidden,
    /// Nothing here, or nothing this caller may see (404).
    #[error("not found")]
    NotFound,
    /// A state conflict (409), with its code.
    #[error("conflict {0}")]
    Conflict(&'static str),
    /// A plan limit (403), naming which.
    #[error("plan limit {0}")]
    PlanLimit(&'static str),
    /// No password slot within the wait (503).
    #[error("overloaded")]
    Overloaded,
    /// The database is unavailable (503).
    #[error("database unavailable")]
    Unavailable,
    /// Anything else (500), logged once here.
    #[error("internal")]
    Internal(#[source] anyhow::Error),
}

impl From<DbError> for AccountsError {
    fn from(e: DbError) -> Self {
        if e.is_unavailable() {
            Self::Unavailable
        } else {
            Self::Internal(anyhow::anyhow!(xpay_db::chain(&e)))
        }
    }
}

impl From<sqlx::Error> for AccountsError {
    fn from(e: sqlx::Error) -> Self {
        DbError::from(e).into()
    }
}

impl IntoResponse for AccountsError {
    fn into_response(self) -> Response {
        use StatusCode as S;
        match self {
            Self::Invalid(code, message) => error(S::UNPROCESSABLE_ENTITY, code, message),
            Self::InviteInvalid => error(
                S::FORBIDDEN,
                "invite_invalid",
                "the invite code is not valid",
            ),
            Self::EmailTaken => error(
                S::CONFLICT,
                "email_taken",
                "an account with this email exists",
            ),
            Self::TokenInvalid => error(
                S::BAD_REQUEST,
                "token_invalid",
                "the link is not valid; ask for a new one",
            ),
            Self::SignInFailed => {
                error(S::UNAUTHORIZED, "sign_in_failed", "wrong email or password")
            }
            Self::EmailUnverified => {
                error(S::FORBIDDEN, "email_unverified", "verify your email first")
            }
            Self::RateLimited(secs) => {
                let mut response = error(
                    S::TOO_MANY_REQUESTS,
                    "rate_limited",
                    "too many attempts; retry later",
                );
                if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
                    response.headers_mut().insert(header::RETRY_AFTER, value);
                }
                response
            }
            Self::Unauthenticated => error(
                S::UNAUTHORIZED,
                "unauthenticated",
                "sign in or present a token",
            ),
            Self::SessionRequired => error(
                S::FORBIDDEN,
                "session_required",
                "this needs a signed-in person, not a token",
            ),
            Self::Forbidden => error(S::FORBIDDEN, "forbidden", "your role does not allow this"),
            Self::NotFound => error(S::NOT_FOUND, "not_found", "not found"),
            Self::Conflict(code) => error(
                S::CONFLICT,
                code,
                "the request conflicts with the current state",
            ),
            Self::PlanLimit(which) => {
                let mut response = error(S::FORBIDDEN, "plan_limit", "the plan's limit is reached");
                let body = serde_json::json!({ "error": { "code": "plan_limit", "message": "the plan's limit is reached", "limit": which } });
                if let Ok(bytes) = serde_json::to_vec(&body) {
                    *response.body_mut() = axum::body::Body::from(bytes);
                }
                response
            }
            Self::Overloaded => error(
                S::SERVICE_UNAVAILABLE,
                "overloaded",
                "the server is at capacity; retry shortly",
            ),
            Self::Unavailable => error(
                S::SERVICE_UNAVAILABLE,
                "unavailable",
                "the database is unavailable; retry shortly",
            ),
            Self::Internal(cause) => {
                tracing::error!(cause = %cause, "accounts request failed");
                error(S::INTERNAL_SERVER_ERROR, "internal", "internal error")
            }
        }
    }
}
