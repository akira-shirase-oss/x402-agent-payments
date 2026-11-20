//! Roles and databases, created by an administrator before the first migration
//! (Backend Platform D-034, Payment Sandbox D-038).
//!
//! - `xpay_owner` owns the database and every object in it, and runs the migrations
//!   (`xpayctl migrate`). Nothing serves requests as it.
//! - `xpay_app` is what the server runs as: `NOSUPERUSER`, owns nothing, no
//!   `CREATEROLE`/`CREATEDB`/`REPLICATION`/`BYPASSRLS`, no `TEMP` on the database (a
//!   temporary table could shadow a relation in an unpinned function), no `CREATE` on the
//!   schema, and exactly the table privileges the migrations grant. [`crate::guard`]
//!   refuses to run as anything broader.
//!
//! On Neon there is no superuser: run the bootstrap as the project's owner role (a member
//! of `neon_superuser`), which may create roles; the attributes it cannot grant are the
//! ones the runtime role must not have anyway.

use std::str::FromStr;

use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, ConnectOptions, Connection, Executor};

use crate::DbError;

/// The owner, used only for migrations.
pub const OWNER_ROLE: &str = "xpay_owner";
/// The runtime role of the server.
pub const RUNTIME_ROLE: &str = "xpay_app";
/// Serializes concurrent bootstraps (parallel tests) on one cluster.
const BOOTSTRAP_LOCK: i64 = 7_316_101;

/// A plain lower-case identifier of at most 63 bytes.
pub fn valid_identifier(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_password(password: &str) -> bool {
    (16..=128).contains(&password.len())
        && password
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Creates (or updates) both roles and creates `database` owned by the owner, with
/// `CONNECT` for the runtime role and nothing for `PUBLIC` (so no `TEMP`). Idempotent.
pub async fn bootstrap(
    admin_url: &str,
    database: &str,
    owner_password: &str,
    runtime_password: &str,
) -> Result<(), DbError> {
    if !valid_identifier(database) {
        return Err(DbError::Invalid(format!(
            "database name {database:?} is not a plain identifier"
        )));
    }
    if !(valid_password(owner_password) && valid_password(runtime_password)) {
        return Err(DbError::Invalid(
            "role passwords must be 16 to 128 of [A-Za-z0-9_-]".to_owned(),
        ));
    }
    // The ALTER ROLE statements carry both passwords and sqlx logs statements (DEBUG, and
    // WARN when slow), so this connection logs none.
    let mut admin = PgConnectOptions::from_str(admin_url)
        .map_err(DbError::Other)?
        .disable_statement_logging()
        .connect()
        .await?;
    admin
        .execute(AssertSqlSafe(format!(
            "SELECT pg_advisory_lock({BOOTSTRAP_LOCK})"
        )))
        .await?;
    let result = async {
        admin
            .execute(AssertSqlSafe(format!(
                "DO $$ BEGIN
                     IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{OWNER_ROLE}') THEN
                         CREATE ROLE {OWNER_ROLE};
                     END IF;
                     IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{RUNTIME_ROLE}') THEN
                         CREATE ROLE {RUNTIME_ROLE};
                     END IF;
                 END $$"
            )))
            .await?;
        let attributes = "LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS";
        admin
            .execute(AssertSqlSafe(format!(
                "ALTER ROLE {OWNER_ROLE} WITH {attributes} PASSWORD '{owner_password}'"
            )))
            .await?;
        admin
            .execute(AssertSqlSafe(format!(
                "ALTER ROLE {RUNTIME_ROLE} WITH {attributes} NOINHERIT PASSWORD '{runtime_password}'"
            )))
            .await?;
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
                .bind(database)
                .fetch_one(&mut admin)
                .await?;
        if !exists {
            admin
                .execute(AssertSqlSafe(format!(
                    "CREATE DATABASE {database} OWNER {OWNER_ROLE}"
                )))
                .await?;
        }
        admin
            .execute(AssertSqlSafe(format!(
                "REVOKE ALL ON DATABASE {database} FROM PUBLIC"
            )))
            .await?;
        admin
            .execute(AssertSqlSafe(format!(
                "GRANT CONNECT ON DATABASE {database} TO {RUNTIME_ROLE}"
            )))
            .await?;
        Ok::<(), DbError>(())
    }
    .await;
    let _ = admin
        .execute(AssertSqlSafe(format!(
            "SELECT pg_advisory_unlock({BOOTSTRAP_LOCK})"
        )))
        .await;
    admin.close().await.ok();
    result
}

/// Drops `database`, forcing out its connections.
pub async fn drop_database(admin_url: &str, database: &str) -> Result<(), DbError> {
    if !valid_identifier(database) {
        return Err(DbError::Invalid(format!(
            "database name {database:?} is not a plain identifier"
        )));
    }
    let mut admin = PgConnectOptions::from_str(admin_url)
        .map_err(DbError::Other)?
        .connect()
        .await?;
    admin
        .execute(AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {database} WITH (FORCE)"
        )))
        .await?;
    admin.close().await.ok();
    Ok(())
}

/// `url` with its user, password and database replaced (for building the owner and
/// runtime URLs from an administrator's URL).
pub fn role_url(url: &str, role: &str, password: &str, database: &str) -> Result<String, DbError> {
    let bad = || {
        DbError::Invalid("a database URL must look like postgres://user@host[:port]/db".to_owned())
    };
    let (scheme, rest) = url.split_once("://").ok_or_else(bad)?;
    let (authority, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let query = tail.split_once('?').map(|(_, q)| q);
    let mut out = format!("{scheme}://{role}:{password}@{host}/{database}");
    if let Some(query) = query {
        out.push('?');
        out.push_str(query);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_urls_replace_user_password_and_database() {
        assert_eq!(
            role_url(
                "postgres://admin:secret@127.0.0.1:63332/postgres",
                RUNTIME_ROLE,
                "pw-1234567890abcdef",
                "app_x"
            )
            .ok(),
            Some("postgres://xpay_app:pw-1234567890abcdef@127.0.0.1:63332/app_x".to_owned())
        );
        assert_eq!(
            role_url(
                "postgresql://u@db.local/x?sslmode=require",
                OWNER_ROLE,
                "p",
                "y"
            )
            .ok(),
            Some("postgresql://xpay_owner:p@db.local/y?sslmode=require".to_owned())
        );
        assert!(role_url("not a url", OWNER_ROLE, "p", "y").is_err());
    }

    #[test]
    fn names_and_passwords_are_plain() {
        assert!(valid_identifier("xpay_test_0190"));
        assert!(!valid_identifier("x; DROP"));
        assert!(!valid_identifier("Upper"));
        assert!(valid_password("xpay-app-dev-password"));
        assert!(!valid_password("short"));
        assert!(!valid_password("quote'here-1234567890"));
    }
}
