//! Per-table row counts and checksums: the pass condition of the restore drills and the
//! upgrade rehearsal (before and after must match). A table's checksum is the md5 of its
//! rows' md5s in sorted order, so it does not depend on physical order or on keys.

use std::collections::BTreeMap;

use serde::Serialize;
use sqlx::{AssertSqlSafe, PgPool};

use crate::DbError;
use crate::roles::valid_identifier;

/// One table's fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableFingerprint {
    /// Rows.
    pub rows: i64,
    /// md5 over the sorted row digests (empty for an empty table).
    pub md5: String,
}

/// Every ordinary table in `public`, by name.
pub async fn fingerprint(pool: &PgPool) -> Result<BTreeMap<String, TableFingerprint>, DbError> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p') ORDER BY 1",
    )
    .fetch_all(pool)
    .await?;
    let mut out = BTreeMap::new();
    for table in tables {
        if !valid_identifier(&table) {
            return Err(DbError::Invalid(format!("unexpected table name {table:?}")));
        }
        let (rows, md5): (i64, String) = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT count(*), coalesce(md5(string_agg(d, '' ORDER BY d)), '')
               FROM (SELECT md5(t::text) AS d FROM public.{table} t) rows"
        )))
        .fetch_one(pool)
        .await?;
        out.insert(table, TableFingerprint { rows, md5 });
    }
    Ok(out)
}
