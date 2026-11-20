//! The x402 buyer. [`Wallet::get`] fetches a URL; on a 402 it picks an offer it may pay
//! (an allowlisted `https` origin, or `http` on loopback; that origin's own `payTo` list;
//! known USDC on an allowed network; within the per-payment cap and the remaining total
//! cap), reserves the amount, signs an EIP-3009 authorisation (EIP-712, alloy) and retries
//! with `PAYMENT-SIGNATURE`. A retry after a lost answer resends the SAME proof, never a
//! new one: one 402, at most one payment.
//!
//! Once a proof has left the process no answer from the seller releases its reservation:
//! anyone holding the proof can settle it. The reservation stays [`Unresolved`] until
//! [`Wallet::reconcile`] sees, on chain, that the authorisation expired unused
//! (`validBefore` passed and `authorizationState(from, nonce)` is still false).

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
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

/// Local clocks drift from block time: an authorisation counts as expired this long
/// after its `validBefore`.
pub const EXPIRY_MARGIN_SECS: u64 = 120;

/// One origin the wallet may pay, and the only recipients it may pay there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedHost {
    /// `host` or `host:port`, compared exactly; a bare host means the scheme's default
    /// port (443, or 80 for loopback `http`), never any port.
    pub host: String,
    pub pay_to: Vec<Address>,
}

impl AllowedHost {
    pub fn new(host: impl Into<String>, pay_to: Vec<Address>) -> Self {
        Self {
            host: host.into(),
            pay_to,
        }
    }

    fn matches(&self, host: &str, port: u16, default: u16) -> bool {
        let entry = self.host.to_ascii_lowercase();
        match entry.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
                h == host && p.parse() == Ok(port)
            }
            _ => entry == host && port == default,
        }
    }
}

/// What the wallet may pay, and how much.
#[derive(Debug, Clone)]
pub struct Policy {
    pub allow_hosts: Vec<AllowedHost>,
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
    /// Bound: proofs sent whose outcome is not yet known on chain.
    pub max_unresolved: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            max_body_bytes: 1024 * 1024,
            retries: 2,
            max_unresolved: 1024,
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

/// A proof that left the process; its amount stays committed until the chain says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    pub network: Caip2,
    pub asset: Address,
    pub from: Address,
    pub nonce: B256,
    pub valid_before: u64,
    pub amount: U256,
}

/// `authorizationState(from, nonce)` on the USDC contract `asset` of `network`.
pub trait AuthorizationState: Send + Sync {
    fn authorization_state<'a>(
        &'a self,
        network: &'a Caip2,
        asset: Address,
        from: Address,
        nonce: B256,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>>;
}

/// What one [`Wallet::reconcile`] pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Expired unused: the reservation left the cap.
    pub released: usize,
    /// Used on chain: the amount stays spent.
    pub spent: usize,
    /// Not yet expired, or the chain did not answer: still reserved.
    pub waiting: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    #[error("URL is not https (or http on loopback) with a host")]
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
    /// The seller answered 402 to a proof it holds. `0` is the reason code if it is a
    /// plain code, else `unrecognised_reason`; the amount stays reserved.
    #[error("the seller refused the payment ({0}); the amount stays reserved until reconciled")]
    Refused(String),
    #[error("the seller says the payment settled but did not deliver; the amount is spent")]
    PaidNotDelivered(Option<Box<SettleResponse>>),
    #[error("too many payments whose outcome is still unknown; reconcile first")]
    TooManyUnresolved,
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
    /// Bounded by `limits.max_unresolved`.
    unresolved: Mutex<Vec<Unresolved>>,
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
            unresolved: Mutex::new(Vec::new()),
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

    /// Proofs sent whose outcome the chain has not yet settled for this wallet.
    pub fn unresolved(&self) -> Vec<Unresolved> {
        self.unresolved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn allowed(&self, url: &reqwest::Url) -> Result<&AllowedHost, WalletError> {
        let host = url.host_str().ok_or(WalletError::Url)?.to_ascii_lowercase();
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let loopback = bare
            .parse::<IpAddr>()
            .map_or(host == "localhost", |ip| ip.is_loopback());
        let default = match url.scheme() {
            "https" => 443,
            "http" if loopback => 80,
            _ => return Err(WalletError::Url),
        };
        let port = url.port().unwrap_or(default);
        self.policy
            .allow_hosts
            .iter()
            .find(|a| a.matches(&host, port, default))
            .ok_or(WalletError::HostNotAllowed)
    }

    /// The first offer this wallet may pay: exact scheme, an allowed network, that
    /// network's real USDC with its real EIP-712 domain, a recipient on this origin's
    /// list, a non-zero amount, a bounded validity.
    fn choose<'a>(
        &self,
        required: &'a PaymentRequired,
        origin: &AllowedHost,
    ) -> Result<(&'a PaymentRequirements, U256), WalletError> {
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
            let pay_to_ok = Address::from_str(&o.pay_to).is_ok_and(|a| origin.pay_to.contains(&a));
            let fit = o.scheme == SCHEME_EXACT
                && self.policy.networks.contains(&o.network)
                && o.asset.eq_ignore_ascii_case(usdc.address)
                && domain_ok
                && pay_to_ok
                && o.max_timeout_seconds <= self.policy.max_timeout_seconds;
            if !fit {
                continue;
            }
            match U256::from_str_radix(&o.amount, 10) {
                Ok(a) if a.is_zero() => {}
                Ok(a) if a <= self.policy.max_per_payment => return Ok((o, a)),
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

    /// Releases what expired unused on chain; forgets what was used (it stays spent).
    /// Entries the chain does not answer for stay reserved.
    pub async fn reconcile(&self, chain: &dyn AuthorizationState) -> Reconciled {
        let now = self.clock.now();
        let mut done = Reconciled::default();
        for u in self.unresolved() {
            if now <= u.valid_before.saturating_add(EXPIRY_MARGIN_SECS) {
                done.waiting += 1;
                continue;
            }
            let state = tokio::time::timeout(
                self.limits.request_timeout,
                chain.authorization_state(&u.network, u.asset, u.from, u.nonce),
            )
            .await;
            let Ok(Ok(used)) = state else {
                done.waiting += 1;
                continue;
            };
            if !self.forget(u.nonce) {
                continue;
            }
            if used {
                done.spent += 1;
            } else {
                self.release(u.amount);
                done.released += 1;
            }
        }
        done
    }

    fn forget(&self, nonce: B256) -> bool {
        let mut list = self
            .unresolved
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let before = list.len();
        list.retain(|u| u.nonce != nonce);
        list.len() != before
    }

    fn track(&self, u: Unresolved) -> Result<(), WalletError> {
        let mut list = self
            .unresolved
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if list.len() >= self.limits.max_unresolved {
            return Err(WalletError::TooManyUnresolved);
        }
        list.push(u);
        Ok(())
    }

    /// GET `url`, paying its 402 if policy allows.
    pub async fn get(&self, url: &str) -> Result<Receipt, WalletError> {
        let url = reqwest::Url::parse(url).map_err(|_| WalletError::Url)?;
        let origin = self.allowed(&url)?;
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
        let (offer, amount) = self.choose(&required, origin)?;
        let asset = Address::from_str(&offer.asset).map_err(|_| WalletError::NoAcceptableOffer)?;
        self.reserve(amount)?;
        let nonce = B256::random();
        let now = self.clock.now();
        let pending = Unresolved {
            network: offer.network.clone(),
            asset,
            from: self.address,
            nonce,
            valid_before: now.saturating_add(offer.max_timeout_seconds),
            amount,
        };
        let proof = self.track(pending).and_then(|()| {
            exact::pay(
                self.signer.as_ref(),
                self.address,
                offer,
                Some(required.resource.clone()),
                now,
                nonce,
            )
            .and_then(|p| encode_header(&p).map_err(|e| SignError::Signer(e.to_string())))
            .map_err(|e| {
                self.forget(nonce);
                e.into()
            })
        });
        let proof = match proof {
            Ok(p) => p,
            Err(e) => {
                // Nothing left the process: the reservation goes back.
                self.release(amount);
                return Err(e);
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
                self.forget(nonce);
                return Ok(Receipt {
                    status: status.as_u16(),
                    body,
                    paid: Some(amount),
                    settlement,
                });
            }
            if status == StatusCode::CONFLICT && settlement.as_ref().is_some_and(|s| s.success) {
                return Err(WalletError::PaidNotDelivered(settlement.map(Box::new)));
            }
            if status == StatusCode::PAYMENT_REQUIRED {
                let why = headers
                    .get(PAYMENT_REQUIRED_HEADER)
                    .and_then(|h| decode_header::<PaymentRequired>(h.as_bytes()).ok())
                    .and_then(|r| r.error);
                return Err(WalletError::Refused(reason_code(why.as_deref())));
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

/// A seller's reason, kept only if it is a plain code: it ends up in agent logs and prompts.
fn reason_code(why: Option<&str>) -> String {
    match why {
        Some(w)
            if (1..=64).contains(&w.len())
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') =>
        {
            w.to_owned()
        }
        _ => "unrecognised_reason".to_owned(),
    }
}
