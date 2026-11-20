//! Test support: one throwaway, migrated database per test, set up exactly like
//! production — created by the administrator, owned and migrated by the owner role, used
//! through the runtime role.
//!
//! `XPAY_TEST_DATABASE_URL` names a PostgreSQL role allowed to create roles and
//! databases (the local stack's administrator: `scripts/stack.sh env` prints it). Tests
//! that need a database fail loudly without it rather than pass silently.
#![expect(
    clippy::expect_used,
    reason = "test support: a setup failure must abort the test with its cause"
)]

use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{AssertSqlSafe, ConnectOptions, Connection, Executor};
use uuid::Uuid;

use crate::roles::{OWNER_ROLE, RUNTIME_ROLE, bootstrap, role_url};

/// The variable naming the administrator's connection.
pub const ENV: &str = "XPAY_TEST_DATABASE_URL";
/// Development password of the owner role (a public value, like the stack's).
pub const OWNER_PASSWORD: &str = "xpay-owner-dev-password";
/// Development password of the runtime role (a public value, like the stack's).
pub const RUNTIME_PASSWORD: &str = "xpay-app-dev-password";

/// Which role a connection uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The administrator (a superuser in the local stack).
    Admin,
    /// The owner.
    Owner,
    /// The runtime role.
    Runtime,
}

/// A freshly created and migrated database, dropped by [`TestDb::cleanup`].
#[derive(Debug)]
pub struct TestDb {
    /// A pool on the new database, as the runtime role.
    pub pool: PgPool,
    admin_url: String,
    name: String,
}

impl TestDb {
    /// Creates `xpay_test_<uuid>`, migrates it as the owner, connects as the runtime role.
    ///
    /// # Panics
    ///
    /// When `XPAY_TEST_DATABASE_URL` is unset or the server is unreachable: a database
    /// test that cannot run must fail, not pass.
    pub async fn new() -> Self {
        let db = Self::unmigrated().await;
        let owner = db.connect_as(Role::Owner).await;
        crate::MIGRATOR
            .run(&owner)
            .await
            .expect("migrating the test database as the owner");
        owner.close().await;
        db
    }

    /// Creates the database and roles without migrating.
    pub async fn unmigrated() -> Self {
        let admin_url = std::env::var(ENV).unwrap_or_else(|_| {
            panic!("{ENV} is not set; start the stack with `scripts/stack.sh up` and export it")
        });
        let name = format!("xpay_test_{}", Uuid::now_v7().simple());
        bootstrap(&admin_url, &name, OWNER_PASSWORD, RUNTIME_PASSWORD)
            .await
            .expect("bootstrapping the test database and roles");
        let db = Self {
            pool: PgPoolOptions::new().connect_lazy_with(
                PgConnectOptions::from_str(&admin_url).expect("invariant: the admin URL parses"),
            ),
            admin_url,
            name,
        };
        Self {
            pool: db.connect_as(Role::Runtime).await,
            ..db
        }
    }

    /// The database's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The URL for `role` on this database.
    pub fn url_as(&self, role: Role) -> String {
        match role {
            Role::Admin => {
                let (base, query) = match self.admin_url.split_once('?') {
                    Some((base, query)) => (base, Some(query)),
                    None => (self.admin_url.as_str(), None),
                };
                let prefix = base.rsplit_once('/').map_or(base, |(prefix, _)| prefix);
                match query {
                    Some(query) => format!("{prefix}/{}?{query}", self.name),
                    None => format!("{prefix}/{}", self.name),
                }
            }
            Role::Owner => role_url(&self.admin_url, OWNER_ROLE, OWNER_PASSWORD, &self.name)
                .expect("invariant: the admin URL is a postgres URL"),
            Role::Runtime => role_url(&self.admin_url, RUNTIME_ROLE, RUNTIME_PASSWORD, &self.name)
                .expect("invariant: the admin URL is a postgres URL"),
        }
    }

    /// The runtime role's URL (for child processes).
    pub fn url(&self) -> String {
        self.url_as(Role::Runtime)
    }

    /// A small pool on this database as `role`.
    pub async fn connect_as(&self, role: Role) -> PgPool {
        let options =
            crate::pool::connect_options(&self.url_as(role), &crate::PoolSettings::default())
                .expect("invariant: the test URL parses");
        PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options)
            .await
            .expect("connecting to the test database")
    }

    /// Runs `sql` as `role` (setup and weakening in tests).
    pub async fn exec_as(&self, role: Role, sql: &str) {
        let pool = self.connect_as(role).await;
        pool.execute(AssertSqlSafe(sql.to_owned()))
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        pool.close().await;
    }

    /// Drops the database, forcing out any connection a test or a child left behind.
    pub async fn cleanup(self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.pool.close()).await;
        if let Ok(mut admin) = PgConnectOptions::from_str(&self.admin_url)
            .expect("invariant: the admin URL parses")
            .connect()
            .await
        {
            let _ = admin
                .execute(AssertSqlSafe(format!(
                    "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                    self.name
                )))
                .await;
            admin.close().await.ok();
        }
    }
}
