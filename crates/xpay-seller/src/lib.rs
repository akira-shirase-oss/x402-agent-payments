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
pub use layer::{Paid, PaymentConfig, PaymentLayer, PaymentService, SettleMode};
pub use store::{Claim, ClaimState, MemoryNonceStore, NonceKey, NonceStore};
pub use xpay_x402::facilitator::{Facilitator, FacilitatorError};
