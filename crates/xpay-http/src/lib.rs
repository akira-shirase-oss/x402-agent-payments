//! The bounded HTTP layer every copy of the template keeps.
//!
//! - [`serve`]: the accept loop, with a bound on every way one client can hold a
//!   connection (header-read and idle timeout, write stall, requests per connection,
//!   lifetime) and a cap on connections.
//! - [`stack`]: the Tower middleware stack (request ids, one error envelope, panics as
//!   500s, a response deadline, body limits), load shedding and the server-error window.
//! - [`client`]: who a request comes from (the TCP peer, or a trusted proxy's hop).
//! - [`budget`]: bounded token-bucket tables, per client and global.
//! - [`shutdown`]: the signals that start a drain.
//!
//! Each limit is proven by a test that fails when the limit is removed:
//! `scripts/prove-limits.sh` removes each one (the lines marked `LIMIT(...)`) in a scratch
//! copy and requires its test to fail.

pub mod budget;
pub mod client;
pub mod serve;
pub mod shutdown;
pub mod stack;

pub use serve::{ConnectionLimits, Drained, serve};
