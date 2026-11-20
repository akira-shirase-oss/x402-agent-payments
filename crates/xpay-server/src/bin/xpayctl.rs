//! `xpayctl`: the operator's tool. Each command reads its secrets the way the server
//! does (credential, `_FILE`, or — in development — the variable).
//!
//! ```text
//! xpayctl bootstrap                 admin: create the roles and the database
//! xpayctl migrate                   owner: apply this binary's migrations (forward only)
//! xpayctl schema-status             any role: this binary vs the database; exit 4 unless current
//! xpayctl check                     runtime role: the startup check; exit 3 on a refusal
//! xpayctl digests                   owner: trigger and function digests for the manifest
//! xpayctl fingerprint               owner: per-table rows and checksums (JSON)
//! xpayctl invite [COUNT] [HOURS]    runtime role: print COUNT invite codes valid HOURS
//! xpayctl prune-events DAYS         owner: delete security events older than DAYS (>= 30)
//! ```

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, bail};
use sqlx::PgPool;
use xpay_accounts::{store, tokens};
use xpay_db::guard::{Refusal, check_runtime, digests};
use xpay_db::schema::{self, SchemaStatus};
use xpay_db::{MIGRATOR, PoolSettings, fingerprint, roles};
use xpay_server::config::{Process, secret};

fn required(credential: &str, variable: &str) -> anyhow::Result<String> {
    match secret(&Process, credential, variable).map_err(anyhow::Error::msg)? {
        Some(value) => Ok(value.expose().to_owned()),
        None => {
            bail!("{variable} is required (credential {credential}, {variable}_FILE or {variable})")
        }
    }
}

/// A small pool with long statement deadlines (migrations, fingerprints), still bounded.
fn pool(credential: &str, variable: &str) -> anyhow::Result<PgPool> {
    let url = required(credential, variable)?;
    let settings = PoolSettings {
        max_connections: 2,
        statement_timeout: Duration::from_secs(300),
        lock_timeout: Duration::from_secs(10),
        acquire_timeout: Duration::from_secs(10),
    };
    xpay_db::connect_lazy(&url, &settings).map_err(|e| anyhow::anyhow!(xpay_db::chain(&e)))
}

fn owner() -> anyhow::Result<PgPool> {
    pool("migration-database-url", "XPAY_MIGRATION_DATABASE_URL")
}

fn runtime() -> anyhow::Result<PgPool> {
    pool("database-url", "XPAY_DATABASE_URL")
}

fn db(e: &xpay_db::DbError) -> anyhow::Error {
    anyhow::anyhow!(xpay_db::chain(e))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        eprintln!("xpayctl: could not start the runtime");
        return ExitCode::FAILURE;
    };
    let arg = |i: usize| args.get(i).map(String::as_str);
    let outcome = runtime.block_on(async {
        match arg(0) {
            Some("bootstrap") => bootstrap().await,
            Some("migrate") => migrate().await,
            Some("schema-status") => schema_status().await,
            Some("check") => check().await,
            Some("digests") => print_digests().await,
            Some("fingerprint") => print_fingerprint().await,
            Some("invite") => invite(arg(1), arg(2)).await,
            Some("prune-events") => prune_events(arg(1)).await,
            _ => {
                eprintln!("usage: xpayctl bootstrap|migrate|schema-status|check|digests|fingerprint|invite [COUNT] [HOURS]|prune-events DAYS");
                Ok(ExitCode::from(2))
            }
        }
    });
    outcome.unwrap_or_else(|e| {
        eprintln!("xpayctl: {e:#}");
        ExitCode::FAILURE
    })
}

async fn bootstrap() -> anyhow::Result<ExitCode> {
    let admin = required("admin-database-url", "XPAY_ADMIN_DATABASE_URL")?;
    let name = std::env::var("XPAY_DATABASE_NAME").context("XPAY_DATABASE_NAME is required")?;
    let owner = required("owner-password", "XPAY_OWNER_PASSWORD")?;
    let app = required("app-password", "XPAY_APP_PASSWORD")?;
    roles::bootstrap(&admin, &name, &owner, &app)
        .await
        .map_err(|e| db(&e))?;
    println!(
        "bootstrap ok: roles {} and {}, database {name}",
        roles::OWNER_ROLE,
        roles::RUNTIME_ROLE
    );
    Ok(ExitCode::SUCCESS)
}

/// Forward only, and only on this binary's line: a newer, modified or failed ledger is
/// refused before anything runs.
async fn migrate() -> anyhow::Result<ExitCode> {
    let pool = owner()?;
    let before = schema::status(&pool, &MIGRATOR).await.map_err(|e| db(&e))?;
    if matches!(
        before,
        SchemaStatus::Newer { .. } | SchemaStatus::Modified { .. } | SchemaStatus::Failed { .. }
    ) {
        println!("{}", serde_json::to_string(&before)?);
        eprintln!(
            "xpayctl: refusing to migrate: the database is not on this binary's line of migrations"
        );
        return Ok(ExitCode::from(4));
    }
    MIGRATOR.run(&pool).await.context("migrating")?;
    let after = schema::status(&pool, &MIGRATOR).await.map_err(|e| db(&e))?;
    println!("{}", serde_json::to_string(&after)?);
    Ok(ExitCode::SUCCESS)
}

async fn schema_status() -> anyhow::Result<ExitCode> {
    let pool = if secret(
        &Process,
        "migration-database-url",
        "XPAY_MIGRATION_DATABASE_URL",
    )
    .map_err(anyhow::Error::msg)?
    .is_some()
    {
        owner()?
    } else {
        runtime()?
    };
    let status = schema::status(&pool, &MIGRATOR).await.map_err(|e| db(&e))?;
    println!("{}", serde_json::to_string(&status)?);
    Ok(if status.is_current() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(4)
    })
}

async fn check() -> anyhow::Result<ExitCode> {
    match check_runtime(&runtime()?).await {
        Ok(()) => {
            println!("check ok: the role and the protections are as the migrations left them");
            Ok(ExitCode::SUCCESS)
        }
        Err(Refusal::Db(e)) => Err(db(&e)),
        Err(refused) => {
            println!("{refused}");
            Ok(ExitCode::from(3))
        }
    }
}

async fn print_digests() -> anyhow::Result<ExitCode> {
    for (kind, name, digest) in digests(&owner()?).await.map_err(|e| db(&e))? {
        println!("{kind} {name} {digest}");
    }
    Ok(ExitCode::SUCCESS)
}

async fn print_fingerprint() -> anyhow::Result<ExitCode> {
    let tables = fingerprint::fingerprint(&owner()?)
        .await
        .map_err(|e| db(&e))?;
    println!("{}", serde_json::to_string(&tables)?);
    Ok(ExitCode::SUCCESS)
}

async fn invite(count: Option<&str>, hours: Option<&str>) -> anyhow::Result<ExitCode> {
    let count: usize = count
        .unwrap_or("1")
        .parse()
        .context("COUNT must be a number")?;
    let hours: u64 = hours
        .unwrap_or("168")
        .parse()
        .context("HOURS must be a number")?;
    if !(1..=100).contains(&count) || !(1..=720).contains(&hours) {
        bail!("COUNT must be 1..=100 and HOURS 1..=720");
    }
    let codes: Vec<String> = (0..count)
        .map(|_| tokens::new_invite())
        .collect::<Result<_, _>>()?;
    let digests: Vec<[u8; 32]> = codes.iter().map(|c| tokens::digest(c)).collect();
    store::create_invites(&runtime()?, &digests, Duration::from_hours(hours))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    for code in codes {
        println!("{code}");
    }
    Ok(ExitCode::SUCCESS)
}

/// The owner switches the append-only trigger off and on inside one transaction, so no
/// other session ever sees it off; the manifest holds again at commit.
async fn prune_events(days: Option<&str>) -> anyhow::Result<ExitCode> {
    let days: i32 = days
        .context("DAYS is required")?
        .parse()
        .context("DAYS must be a number")?;
    if days < 30 {
        bail!("DAYS must be at least 30");
    }
    let pool = owner()?;
    let mut tx = pool.begin().await?;
    sqlx::query("ALTER TABLE account_events DISABLE TRIGGER account_events_append_only")
        .execute(&mut *tx)
        .await?;
    let pruned =
        sqlx::query("DELETE FROM account_events WHERE at < now() - make_interval(days => $1)")
            .bind(days)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    sqlx::query("ALTER TABLE account_events ENABLE ALWAYS TRIGGER account_events_append_only")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    println!("pruned {pruned} events older than {days} days");
    Ok(ExitCode::SUCCESS)
}
