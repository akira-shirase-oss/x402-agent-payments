//! A local x402 facilitator for tests and development: no chain, no network, Base Sepolia only.
//!
//! `verify` runs every offline exact-EVM check (signature recovery under the network's USDC
//! domain included) and the nonce ledger; `settle` does the same, then records the
//! `(payer, nonce)` as used, exactly once, and answers a deterministic transaction hash.
//! A re-settle of the very same authorisation answers the same transaction (the chain would
//! show it landed once); a different authorisation on a used nonce is refused.
//!
//! Faults ([`Faults`]) make the failure paths reachable on demand: a slow or hanging
//! settle, a `settlement_pending` answer, a transport error, a refusal.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use alloy_primitives::{Address, B256, keccak256};
use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use xpay_x402::clock::{Clock, SystemClock};
use xpay_x402::exact::{self, USDC};
use xpay_x402::facilitator::{BoxFuture, Facilitator, FacilitatorError};
use xpay_x402::{
    Caip2, FacilitatorRequest, SCHEME_EXACT, SettleResponse, SupportedKind, SupportedResponse,
    VerifyResponse, X402_VERSION, reason,
};

/// Bound: the largest `/verify` or `/settle` body the HTTP mock reads.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Failure injection. Each applies to every call until changed.
#[derive(Debug, Clone, Default)]
pub struct Faults {
    /// Settle waits this long before answering (`Duration::MAX`-like values: it hangs).
    pub settle_delay: Option<Duration>,
    /// Settle records the nonce as used, then answers `settlement_pending`.
    pub settle_pending: bool,
    /// Settle records the nonce, then fails as a transport error would (outcome unknown).
    pub settle_lost_answer: bool,
    /// Settle refuses with this reason without touching the ledger (e.g. insufficient funds).
    pub settle_refuse: Option<&'static str>,
    /// Verify fails as a transport error would.
    pub verify_unreachable: bool,
}

#[derive(Debug, Default)]
struct Ledger {
    /// (payer, nonce) → (signature, transaction): what landed.
    used: HashMap<(Address, B256), (String, String)>,
}

/// The mock. Cheap to clone; clones share the ledger and the faults.
#[derive(Clone)]
pub struct MockFacilitator {
    ledger: Arc<Mutex<Ledger>>,
    faults: Arc<Mutex<Faults>>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for MockFacilitator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockFacilitator").finish_non_exhaustive()
    }
}

impl Default for MockFacilitator {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}

impl MockFacilitator {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            ledger: Arc::default(),
            faults: Arc::default(),
            clock,
        }
    }

    pub fn set_faults(&self, faults: Faults) {
        *self.faults.lock().unwrap_or_else(PoisonError::into_inner) = faults;
    }

    fn faults(&self) -> Faults {
        self.faults
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many authorisations have landed.
    pub fn settled_count(&self) -> usize {
        self.ledger
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .used
            .len()
    }

    fn check(&self, req: &FacilitatorRequest) -> Result<exact::Verified, &'static str> {
        if req.x402_version != X402_VERSION {
            return Err(reason::VERSION);
        }
        // Development only: no chain, no balances, so never a network where money is real.
        if req.payment_requirements.network.as_str() != Caip2::BASE_SEPOLIA {
            return Err(reason::NETWORK);
        }
        let d = exact::usdc(&req.payment_requirements.network).ok_or(reason::NETWORK)?;
        if !req
            .payment_requirements
            .asset
            .eq_ignore_ascii_case(d.address)
        {
            return Err(reason::REQUIREMENTS);
        }
        exact::check(
            &req.payment_payload,
            &req.payment_requirements,
            self.clock.now(),
        )
    }

    fn verify_now(&self, req: &FacilitatorRequest) -> VerifyResponse {
        let outcome = self.check(req).and_then(|v| {
            let ledger = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
            if ledger.used.contains_key(&(v.payer, v.nonce)) {
                Err(reason::NONCE_REUSED)
            } else {
                Ok(v)
            }
        });
        match outcome {
            Ok(v) => VerifyResponse {
                is_valid: true,
                invalid_reason: None,
                invalid_message: None,
                payer: Some(v.payer.to_checksum(None)),
                extensions: None,
            },
            Err(r) => VerifyResponse {
                is_valid: false,
                invalid_reason: Some(r.into()),
                invalid_message: None,
                payer: None,
                extensions: None,
            },
        }
    }

    /// Lands the authorisation (once) and answers its transaction.
    fn land(&self, req: &FacilitatorRequest) -> Result<(exact::Verified, String), &'static str> {
        let v = self.check(req)?;
        let signature = req
            .payment_payload
            .payload
            .get("signature")
            .and_then(serde_json::Value::as_str)
            .ok_or(reason::PAYLOAD)?
            .to_owned();
        let mut ledger = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        match ledger.used.get(&(v.payer, v.nonce)) {
            Some((sig, tx)) if *sig == signature => Ok((v, tx.clone())),
            Some(_) => Err(reason::NONCE_REUSED),
            None => {
                let tx = keccak256([v.payer.as_slice(), v.nonce.as_slice()].concat()).to_string();
                ledger
                    .used
                    .insert((v.payer, v.nonce), (signature, tx.clone()));
                Ok((v, tx))
            }
        }
    }

    async fn settle_now(
        &self,
        req: FacilitatorRequest,
    ) -> Result<SettleResponse, FacilitatorError> {
        let faults = self.faults();
        if let Some(d) = faults.settle_delay {
            tokio::time::sleep(d).await;
        }
        let network = req.payment_requirements.network.clone();
        let refused = |r: &str| SettleResponse {
            success: false,
            error_reason: Some(r.into()),
            error_message: None,
            payer: None,
            transaction: String::new(),
            network: network.clone(),
            amount: None,
            extensions: None,
        };
        if let Some(r) = faults.settle_refuse {
            return Ok(refused(r));
        }
        let (v, tx) = match self.land(&req) {
            Ok(landed) => landed,
            Err(r) => return Ok(refused(r)),
        };
        if faults.settle_lost_answer {
            return Err(FacilitatorError::Transport("answer lost (injected)".into()));
        }
        if faults.settle_pending {
            return Ok(SettleResponse {
                payer: Some(v.payer.to_checksum(None)),
                ..refused(reason::SETTLEMENT_PENDING)
            });
        }
        Ok(SettleResponse {
            success: true,
            error_reason: None,
            error_message: None,
            payer: Some(v.payer.to_checksum(None)),
            transaction: tx,
            network,
            amount: Some(v.value.to_string()),
            extensions: None,
        })
    }

    pub fn supported() -> SupportedResponse {
        SupportedResponse {
            kinds: USDC
                .iter()
                .filter(|d| d.network == Caip2::BASE_SEPOLIA)
                .filter_map(|d| Caip2::parse(d.network).ok())
                .map(|network| SupportedKind {
                    x402_version: X402_VERSION,
                    scheme: SCHEME_EXACT.into(),
                    network,
                    extra: None,
                })
                .collect(),
            extensions: Vec::new(),
            signers: serde_json::Map::new(),
        }
    }

    /// The facilitator HTTP API: `POST /verify`, `POST /settle`, `GET /supported`.
    pub fn router(self) -> Router {
        Router::new()
            .route("/verify", post(http_verify))
            .route("/settle", post(http_settle))
            .route(
                "/supported",
                get(|| async { axum::Json(Self::supported()) }),
            )
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
            .with_state(self)
    }
}

impl Facilitator for MockFacilitator {
    fn verify(
        &self,
        req: FacilitatorRequest,
    ) -> BoxFuture<'_, Result<VerifyResponse, FacilitatorError>> {
        Box::pin(async move {
            if self.faults().verify_unreachable {
                return Err(FacilitatorError::Transport("unreachable (injected)".into()));
            }
            Ok(self.verify_now(&req))
        })
    }

    fn settle(
        &self,
        req: FacilitatorRequest,
    ) -> BoxFuture<'_, Result<SettleResponse, FacilitatorError>> {
        Box::pin(self.settle_now(req))
    }
}

fn answer<T: serde::Serialize>(r: Result<T, FacilitatorError>) -> Response {
    match r {
        Ok(body) => axum::Json(body).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

async fn http_verify(
    State(m): State<MockFacilitator>,
    axum::Json(req): axum::Json<FacilitatorRequest>,
) -> Response {
    answer(m.verify(req).await)
}

async fn http_settle(
    State(m): State<MockFacilitator>,
    axum::Json(req): axum::Json<FacilitatorRequest>,
) -> Response {
    answer(m.settle(req).await)
}

#[cfg(test)]
mod network_tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256};
    use xpay_x402::clock::ManualClock;

    #[test]
    fn it_supports_base_sepolia_only() {
        let kinds = MockFacilitator::supported().kinds;
        assert_ne!(kinds.len(), 0);
        assert!(
            kinds
                .iter()
                .all(|k| k.network.as_str() == Caip2::BASE_SEPOLIA),
            "{kinds:?}"
        );
    }

    #[tokio::test]
    async fn a_mainnet_payment_is_never_verified_or_settled() {
        let now = 1_800_000_000;
        let mock = MockFacilitator::new(Arc::new(ManualClock::at(now)));
        let signer =
            alloy_signer_local::PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).unwrap();
        let req = exact::usdc_requirements(
            &Caip2::parse(Caip2::BASE).unwrap(),
            U256::from(10_000u64),
            Address::repeat_byte(0x20),
            60,
        )
        .unwrap();
        let payload = exact::pay(
            &signer,
            signer.address(),
            &req,
            None,
            now,
            B256::repeat_byte(1),
        )
        .unwrap();
        let r = FacilitatorRequest {
            x402_version: X402_VERSION,
            payment_payload: payload,
            payment_requirements: req,
        };
        let v = mock.verify(r.clone()).await.unwrap();
        assert_eq!(v.invalid_reason.as_deref(), Some(reason::NETWORK), "{v:?}");
        let s = mock.settle(r).await.unwrap();
        assert!(!s.success, "{s:?}");
        assert_eq!(mock.settled_count(), 0);
    }
}
