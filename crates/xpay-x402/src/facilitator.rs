//! The facilitator contract: `/verify` and `/settle`, as a trait so the seller can run on
//! the HTTP facilitator, the local mock, or a test double.

use std::future::Future;
use std::pin::Pin;

use crate::types::{FacilitatorRequest, SettleResponse, VerifyResponse};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why a facilitator call has no answer. Every variant but `Rejected` means the outcome is
/// unknown: for `settle`, the money may have moved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FacilitatorError {
    #[error("facilitator did not answer in time")]
    Timeout,
    #[error("facilitator unreachable: {0}")]
    Transport(String),
    #[error("facilitator answered status {0}")]
    Status(u16),
    #[error("facilitator answer is not the expected JSON")]
    Malformed,
}

pub trait Facilitator: Send + Sync + 'static {
    fn verify(
        &self,
        req: FacilitatorRequest,
    ) -> BoxFuture<'_, Result<VerifyResponse, FacilitatorError>>;
    fn settle(
        &self,
        req: FacilitatorRequest,
    ) -> BoxFuture<'_, Result<SettleResponse, FacilitatorError>>;
}

impl<F: Facilitator + ?Sized> Facilitator for std::sync::Arc<F> {
    fn verify(
        &self,
        req: FacilitatorRequest,
    ) -> BoxFuture<'_, Result<VerifyResponse, FacilitatorError>> {
        (**self).verify(req)
    }
    fn settle(
        &self,
        req: FacilitatorRequest,
    ) -> BoxFuture<'_, Result<SettleResponse, FacilitatorError>> {
        (**self).settle(req)
    }
}
