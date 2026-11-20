//! Storage errors: unavailability (retry, or answer 503) is kept apart from everything else.

use thiserror::Error;

/// A storage failure.
#[derive(Debug, Error)]
pub enum DbError {
    /// The database could not answer in time (down, pool exhausted, statement or lock
    /// deadline). Callers answer 503 or back off and retry.
    #[error("the database is unavailable")]
    Unavailable(#[source] sqlx::Error),
    /// A value refused before it reached the database.
    #[error("invalid: {0}")]
    Invalid(String),
    /// Anything else.
    #[error("database error")]
    Other(#[source] sqlx::Error),
}

impl From<sqlx::Error> for DbError {
    fn from(error: sqlx::Error) -> Self {
        let unavailable = match &error {
            sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::Protocol(_)
            | sqlx::Error::WorkerCrashed => true,
            // 57014 query_canceled (statement_timeout), 55P03 lock_not_available,
            // 57P01 admin_shutdown, 40001 serialization_failure, 40P01 deadlock_detected.
            sqlx::Error::Database(db) => matches!(
                db.code().as_deref(),
                Some("57014" | "55P03" | "57P01" | "40001" | "40P01")
            ),
            _ => false,
        };
        if unavailable {
            Self::Unavailable(error)
        } else {
            Self::Other(error)
        }
    }
}

impl DbError {
    /// True when retrying later may succeed.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// An error and its sources, `: `-joined, for the one log line where it is handled.
/// (sqlx errors carry no credentials: connection strings are never part of them.)
pub fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// True for a unique-constraint violation (SQLSTATE 23505).
pub fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}
