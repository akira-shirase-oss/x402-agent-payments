//! Binary/schema compatibility. Migrations are forward-only; a binary knows the set it was
//! built with (`MIGRATOR`). At startup, and before the deploy script activates a release,
//! the database's applied set is compared with it:
//!
//! - a migration the binary does not know means the schema is **newer** than the binary:
//!   refused, because the code would run against tables it does not understand. This is
//!   the rollback gate: rolling a binary back past a migration means restoring the
//!   pre-upgrade snapshot first (`docs/OPERATIONS.md`, "Rollback");
//! - a known migration whose checksum differs was **modified** after it shipped: refused;
//! - a failed migration: refused;
//! - a migration the binary needs but the database lacks: **behind**; run
//!   `xpayctl migrate` (as the owner) first.

use std::collections::BTreeMap;

use serde::Serialize;
use sqlx::PgPool;
use sqlx::migrate::Migrator;

use crate::DbError;

/// How the database's applied migrations compare with the binary's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SchemaStatus {
    /// Exactly the binary's migrations, all applied.
    Current {
        /// The latest version.
        version: i64,
    },
    /// Migrations the binary needs are not applied.
    Behind {
        /// The versions not applied yet.
        pending: Vec<i64>,
    },
    /// The database carries migrations this binary does not know.
    Newer {
        /// The unknown versions.
        unknown: Vec<i64>,
    },
    /// An applied migration differs from the binary's copy.
    Modified {
        /// The version.
        version: i64,
    },
    /// A migration is recorded as failed.
    Failed {
        /// The version.
        version: i64,
    },
}

impl SchemaStatus {
    /// True when the binary may serve on this schema.
    pub fn is_current(&self) -> bool {
        matches!(self, Self::Current { .. })
    }
}

/// Compares the database behind `pool` with `migrator`.
pub async fn status(pool: &PgPool, migrator: &Migrator) -> Result<SchemaStatus, DbError> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(pool)
            .await?;
    let applied: Vec<(i64, bool, Vec<u8>)> = if exists {
        sqlx::query_as("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await?
    } else {
        Vec::new()
    };
    let known: BTreeMap<i64, &[u8]> = migrator
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
        .map(|m| (m.version, &*m.checksum))
        .collect();
    let unknown: Vec<i64> = applied
        .iter()
        .map(|(v, _, _)| *v)
        .filter(|v| !known.contains_key(v))
        .collect();
    if !unknown.is_empty() {
        return Ok(SchemaStatus::Newer { unknown });
    }
    for (version, success, checksum) in &applied {
        if !success {
            return Ok(SchemaStatus::Failed { version: *version });
        }
        if known
            .get(version)
            .is_some_and(|k| *k != checksum.as_slice())
        {
            return Ok(SchemaStatus::Modified { version: *version });
        }
    }
    let pending: Vec<i64> = known
        .keys()
        .copied()
        .filter(|v| !applied.iter().any(|(a, _, _)| a == v))
        .collect();
    if !pending.is_empty() {
        return Ok(SchemaStatus::Behind { pending });
    }
    Ok(SchemaStatus::Current {
        version: known.keys().next_back().copied().unwrap_or(0),
    })
}
