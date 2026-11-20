//! The x402 seller side.
//!
//! [`PaymentLayer`] wraps a route: without a payment it answers 402 with `PAYMENT-REQUIRED`;
//! with one it checks it offline, claims its `(asset, payer, nonce)` in a [`NonceStore`],
//! verifies and settles it through a [`Facilitator`], and only then runs the handler
//! (settle-first, the default: see `docs/DECISIONS.md` D-006). A settle whose outcome is
//! unknown (deadline passed, answer lost, `settlement_pending`) HOLDS the claim: the
//! payment is never served and never released, so it cannot be served twice.

pub mod http_facilitator;
pub mod layer;
pub mod store;

pub use http_facilitator::HttpFacilitator;
pub use layer::{
    ConfigError, Paid, PaymentConfig, PaymentLayer, PaymentService, ProxyNet, ProxyNetError,
    SettleMode, default_lock_dir,
};
pub use store::{
    Claim, ClaimState, Delivery, DisputeLog, FileDisputeLog, Listed, MemoryNonceStore, NonceKey,
    NonceStore, Redelivery, Route, drain_to_log,
};
pub use xpay_x402::facilitator::{Facilitator, FacilitatorError};
