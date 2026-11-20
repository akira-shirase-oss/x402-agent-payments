//! The startup check (Backend Platform D-034, Payment Sandbox D-038 and D-058): the server
//! refuses to run
//!
//! - as a role that could switch the protections off: a superuser, the owner of any table
//!   (or a member of it), or a role with `CREATEROLE`, `REPLICATION` or `BYPASSRLS`;
//! - on a database whose protections are not exactly as the migrations left them: the
//!   manifest (`sql/protections.sql`) returns one row per problem. It checks every trigger
//!   present, enabled `ALWAYS` and with its exact definition; every function in `public`
//!   listed, with its exact definition (body included), its search path pinned to
//!   `pg_catalog, pg_temp`, `SECURITY INVOKER` and owned by the owner; the runtime role's
//!   exact privileges on every table; the owner owning every table; and no `TEMP` or
//!   `CREATE` for the runtime role.
//!
//! The manifest is checked absolutely, not by comparison with another copy, so a restore
//! of a weakened source is caught too. A migration that changes a trigger, a function or a
//! grant updates the manifest in the same commit (`xpayctl digests` prints the values
//! on a freshly migrated database).

use sqlx::PgPool;
use thiserror::Error;

use crate::DbError;
use crate::roles::OWNER_ROLE;

/// The protections manifest: one row per problem, none when protected.
pub const PROTECTIONS: &str = include_str!("../sql/protections.sql");

/// Why the server refuses to run.
#[derive(Debug, Error)]
pub enum Refusal {
    /// A superuser can switch the triggers off.
    #[error("refusing to run as {0}: it is a superuser; connect as the runtime role")]
    Superuser(String),
    /// The owner can disable triggers, truncate and rewrite functions.
    #[error("refusing to run as {0}: it owns, or can act as the owner of, the tables")]
    Owner(String),
    /// A role attribute the runtime role must not have.
    #[error("refusing to run as {role}: it has {what}")]
    TooBroad {
        /// The role.
        role: String,
        /// The attribute.
        what: &'static str,
    },
    /// The database's protections were weakened.
    #[error("refusing to run: the database's protections are not as the migrations left them: {0}")]
    Unprotected(String),
    /// The check could not run.
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for Refusal {
    fn from(error: sqlx::Error) -> Self {
        Self::Db(error.into())
    }
}

/// What the manifest finds wrong with the database behind `pool` (empty when protected).
pub async fn protection_problems(pool: &PgPool) -> Result<Vec<String>, DbError> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(PROTECTIONS))
        .fetch_all(pool)
        .await?)
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "a 1:1 image of pg_roles and privilege checks"
)]
#[derive(Debug, sqlx::FromRow)]
struct RoleFacts {
    role: String,
    superuser: bool,
    createrole: bool,
    replication: bool,
    bypassrls: bool,
    owner: bool,
}

/// Checks the role `pool` connects as, then the manifest.
pub async fn check_runtime(pool: &PgPool) -> Result<(), Refusal> {
    let facts = sqlx::query_as::<_, RoleFacts>(
        "SELECT current_user::text AS role,
                r.rolsuper AS superuser, r.rolcreaterole AS createrole,
                r.rolreplication AS replication, r.rolbypassrls AS bypassrls,
                EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                         WHERE n.nspname = 'public'
                           AND pg_has_role(current_user, c.relowner, 'MEMBER'))
                OR pg_has_role(current_user, $1, 'MEMBER') AS owner
           FROM pg_roles r WHERE r.rolname = current_user",
    )
    .bind(OWNER_ROLE)
    .fetch_one(pool)
    .await?;
    let role = facts.role.clone();
    if facts.superuser {
        return Err(Refusal::Superuser(role));
    }
    if facts.owner {
        return Err(Refusal::Owner(role));
    }
    for (has, what) in [
        (facts.createrole, "CREATEROLE"),
        (facts.replication, "REPLICATION"),
        (facts.bypassrls, "BYPASSRLS"),
    ] {
        if has {
            return Err(Refusal::TooBroad { role, what });
        }
    }
    let problems = protection_problems(pool).await?;
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Refusal::Unprotected(problems.join("; ")))
    }
}

/// [`check_runtime`] at process start: retries while the database is unavailable (it may
/// be starting), for at most `deadline`; a refusal fails at once, because no amount of
/// waiting fixes it.
pub async fn check_at_startup(pool: &PgPool, deadline: std::time::Duration) -> Result<(), Refusal> {
    let until = tokio::time::Instant::now() + deadline;
    let mut delay = std::time::Duration::from_millis(200);
    loop {
        match check_runtime(pool).await {
            Err(Refusal::Db(error))
                if error.is_unavailable() && tokio::time::Instant::now() + delay < until =>
            {
                tracing::warn!(
                    cause = crate::chain(&error),
                    "database unavailable; retrying the startup check"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(2));
            }
            other => return other,
        }
    }
}

/// The current digests of every trigger and function in `public`, for updating the
/// manifest after a migration changes one (`xpayctl digests`).
pub async fn digests(pool: &PgPool) -> Result<Vec<(String, String, String)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT 'trigger', c.relname || '.' || t.tgname, md5(pg_get_triggerdef(t.oid))
           FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'public' AND NOT t.tgisinternal
         UNION ALL
         SELECT 'function', p.proname::text, md5(pg_get_functiondef(p.oid))
           FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = 'public'
          ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await?)
}
