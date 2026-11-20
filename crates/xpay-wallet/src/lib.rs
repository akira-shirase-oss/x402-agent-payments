//! The x402 buyer. [`Wallet::get`] fetches a URL; on a 402 it picks an offer it may pay
//! (allowlisted host, known USDC on an allowed network, within the per-payment cap and the
//! remaining total cap), reserves the amount, signs an EIP-3009 authorisation (EIP-712,
//! alloy) and retries with `PAYMENT-SIGNATURE`. A retry after a lost answer resends the
//! SAME proof, never a new one: one 402, at most one payment.
//!
//! The reservation is released only on a definite "not paid" (the seller refused the
//! proof, or settlement failed with a `PAYMENT-RESPONSE` saying so); an unknown outcome
//! keeps it, so the cap holds even when the network lies.

use std::str::FromStr as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use alloy_signer::SignerSync;
use reqwest::StatusCode;
use xpay_x402::clock::Clock;
use xpay_x402::exact::{self, SignError};
use xpay_x402::{
    Caip2, PAYMENT_REQUIRED_HEADER, PAYMENT_RESPONSE_HEADER, PAYMENT_SIGNATURE_HEADER,
    PaymentRequired, PaymentRequirements, SCHEME_EXACT, SettleResponse, decode_header,
    encode_header,
};

/// What the wallet may pay, and how much.
#[derive(Debug, Clone)]
pub struct Policy {
    /// `host` or `host:port`, compared exactly (no suffix or wildcard matching).
    pub allow_hosts: Vec<String>,
    pub networks: Vec<Caip2>,
    /// Atomic USDC units per payment.
    pub max_per_payment: U256,
    /// Atomic USDC units over the wallet's life.
    pub total_cap: U256,
    /// The longest authorisation the wallet signs.
    pub max_timeout_seconds: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub request_timeout: Duration,
    /// Bound: the largest response body read.
    pub max_body_bytes: usize,
    /// Resends of the same proof after a lost answer or a 5xx.
    pub retries: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            max_body_bytes: 1024 * 1024,
            retries: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub status: u16,
    pub body: Vec<u8>,
    /// What was paid, if a payment was made.
    pub paid: Option<U256>,
    pub settlement: Option<SettleResponse>,
}

#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    #[error("URL is not http(s) with a host")]
    Url,
    #[error("host not on the allowlist")]
    HostNotAllowed,
    #[error("the 402 carries no readable requirements")]
    NoRequirements,
    #[error("no offer this wallet may pay (scheme, network, asset or domain)")]
    NoAcceptableOffer,
    #[error("price above the per-payment cap")]
    OverPaymentCap,
    #[error("price above what is left of the total cap")]
    OverTotalCap,
    #[error("signing failed: {0}")]
    Sign(#[from] SignError),
    #[error("the seller refused the payment: {0}")]
    Refused(String),
    #[error("payment outcome unknown after retries (status {0:?}); the amount stays reserved")]
    Unsettled(Option<u16>),
    #[error("response body above the limit")]
    BodyTooLarge,
    #[error("HTTP: {0}")]
    Http(String),
}

pub struct Wallet {
    signer: Arc<dyn SignerSync + Send + Sync>,
    address: Address,
    policy: Policy,
    limits: Limits,
    clock: Arc<dyn Clock>,
    http: reqwest::Client,
    /// Reserved or spent, atomic units.
    committed: Mutex<U256>,
}

impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wallet")
            .field("address", &self.address)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl Wallet {
    pub fn new(
        signer: Arc<dyn SignerSync + Send + Sync>,
        address: Address,
        policy: Policy,
        limits: Limits,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, WalletError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(limits.request_timeout.min(Duration::from_secs(10)))
            .timeout(limits.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| WalletError::Http(e.without_url().to_string()))?;
        Ok(Self {
            signer,
            address,
            policy,
            limits,
            clock,
            http,
            committed: Mutex::new(U256::ZERO),
        })
    }

    pub fn address(&self) -> Address {
        self.address
    }

    /// Reserved or spent so far.
    pub fn committed(&self) -> U256 {
        *self
            .committed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn allowed(&self, url: &reqwest::Url) -> Result<(), WalletError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(WalletError::Url);
        }
        let host = url.host_str().ok_or(WalletError::Url)?;
        let with_port = url.port().map(|p| format!("{host}:{p}"));
        let ok = self
            .policy
            .allow_hosts
            .iter()
            .any(|a| a == host || Some(a) == with_port.as_ref());
        if ok {
            Ok(())
        } else {
            Err(WalletError::HostNotAllowed)
        }
    }

    /// The first offer this wallet may pay: exact scheme, an allowed network, that
    /// network's real USDC with its real EIP-712 domain, a bounded validity.
    fn choose<'a>(
        &self,
        required: &'a PaymentRequired,
    ) -> Result<&'a PaymentRequirements, WalletError> {
        let mut over_cap = false;
        for o in &required.accepts {
            let Some(usdc) = exact::usdc(&o.network) else {
                continue;
            };
            let extra = o.extra.as_ref();
            let domain_ok = extra.and_then(|e| e.get("name")).and_then(|v| v.as_str())
                == Some(usdc.domain_name)
                && extra
                    .and_then(|e| e.get("version"))
                    .and_then(|v| v.as_str())
                    == Some(usdc.domain_version);
            let fit = o.scheme == SCHEME_EXACT
                && self.policy.networks.contains(&o.network)
                && o.asset.eq_ignore_ascii_case(usdc.address)
                && domain_ok
                && Address::from_str(&o.pay_to).is_ok()
                && o.max_timeout_seconds <= self.policy.max_timeout_seconds;
            if !fit {
                continue;
            }
            match U256::from_str_radix(&o.amount, 10) {
                Ok(a) if a <= self.policy.max_per_payment => return Ok(o),
                Ok(_) => over_cap = true,
                Err(_) => {}
            }
        }
        Err(if over_cap {
            WalletError::OverPaymentCap
        } else {
            WalletError::NoAcceptableOffer
        })
    }

    fn reserve(&self, amount: U256) -> Result<(), WalletError> {
        let mut c = self
            .committed
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let next = c.checked_add(amount).ok_or(WalletError::OverTotalCap)?;
        if next > self.policy.total_cap {
            return Err(WalletError::OverTotalCap);
        }
        *c = next;
        Ok(())
    }

    fn release(&self, amount: U256) {
        let mut c = self
            .committed
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *c = c.saturating_sub(amount);
    }

    /// GET `url`, paying its 402 if policy allows.
    pub async fn get(&self, url: &str) -> Result<Receipt, WalletError> {
        let url = reqwest::Url::parse(url).map_err(|_| WalletError::Url)?;
        self.allowed(&url)?;
        let (status, headers, body) = self.send(&url, None).await?;
        if status != StatusCode::PAYMENT_REQUIRED {
            return Ok(Receipt {
                status: status.as_u16(),
                body,
                paid: None,
                settlement: None,
            });
        }
        let required: PaymentRequired = headers
            .get(PAYMENT_REQUIRED_HEADER)
            .and_then(|h| decode_header(h.as_bytes()).ok())
            .ok_or(WalletError::NoRequirements)?;
        let offer = self.choose(&required)?;
        let amount =
            U256::from_str_radix(&offer.amount, 10).map_err(|_| WalletError::NoAcceptableOffer)?;
        self.reserve(amount)?;
        let proof = exact::pay(
            self.signer.as_ref(),
            self.address,
            offer,
            Some(required.resource.clone()),
            self.clock.now(),
            B256::random(),
        )
        .and_then(|p| encode_header(&p).map_err(|e| SignError::Signer(e.to_string())));
        let proof = match proof {
            Ok(p) => p,
            Err(e) => {
                self.release(amount);
                return Err(e.into());
            }
        };
        tracing::info!(amount = %amount, network = %offer.network, "x402 paying");
        let mut last = None;
        for _ in 0..=self.limits.retries {
            let (status, headers, body) = match self.send(&url, Some(&proof)).await {
                Ok(r) => r,
                Err(WalletError::Http(_)) => continue,
                Err(e) => return Err(e),
            };
            let settlement: Option<SettleResponse> = headers
                .get(PAYMENT_RESPONSE_HEADER)
                .and_then(|h| decode_header(h.as_bytes()).ok());
            if status.is_success() {
                return Ok(Receipt {
                    status: status.as_u16(),
                    body,
                    paid: Some(amount),
                    settlement,
                });
            }
            if status == StatusCode::PAYMENT_REQUIRED {
                // A definite refusal: the proof was rejected or settlement failed.
                self.release(amount);
                let why = headers
                    .get(PAYMENT_REQUIRED_HEADER)
                    .and_then(|h| decode_header::<PaymentRequired>(h.as_bytes()).ok())
                    .and_then(|r| r.error)
                    .unwrap_or_default();
                return Err(WalletError::Refused(why));
            }
            last = Some(status.as_u16());
            if status.is_client_error() && status != StatusCode::CONFLICT {
                break;
            }
        }
        Err(WalletError::Unsettled(last))
    }

    async fn send(
        &self,
        url: &reqwest::Url,
        proof: Option<&str>,
    ) -> Result<(StatusCode, reqwest::header::HeaderMap, Vec<u8>), WalletError> {
        let mut req = self.http.get(url.clone());
        if let Some(p) = proof {
            req = req.header(PAYMENT_SIGNATURE_HEADER, p);
        }
        let mut resp = req
            .send()
            .await
            .map_err(|e| WalletError::Http(e.without_url().to_string()))?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| WalletError::Http(e.without_url().to_string()))?
        {
            if body.len() + chunk.len() > self.limits.max_body_bytes {
                return Err(WalletError::BodyTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, headers, body))
    }
}
