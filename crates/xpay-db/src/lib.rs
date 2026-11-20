//! PostgreSQL for a copy of the template (Neon in production, the local stack in
//! development and CI).
//!
//! - [`pool`]: a bounded pool with a deadline on acquiring a connection and on every
//!   statement and lock wait.
//! - [`roles`]: an administrator creates two roles and the database. `xpay_owner` owns
//!   every object and runs the migrations; the server runs as `xpay_app`, which owns
//!   nothing and holds exactly the grants the migrations give it.
//! - [`guard`]: the startup check. The server refuses to run as a role that could switch
//!   the protections off, or on a database whose protections are not exactly as the
//!   migrations left them (`sql/protections.sql`, the manifest).
//! - [`schema`]: binary/schema compatibility. A binary refuses a schema that carries a
//!   migration it does not know, and one that lacks a migration it needs.
//! - [`fingerprint`]: per-table row counts and checksums, the pass condition of the
//!   restore drills and the upgrade rehearsal.

pub mod error;
pub mod fingerprint;
pub mod guard;
#[cfg(feature = "test-support")]
pub mod logcheck;
pub mod pool;
pub mod roles;
pub mod schema;
#[cfg(feature = "test-support")]
pub mod testing;

pub use error::{DbError, chain, is_unique_violation};
pub use pool::{PoolSettings, connect_lazy, connect_options};

/// The embedded migrations (forward-only; never edit one that has shipped).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
