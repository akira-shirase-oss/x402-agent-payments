//! The x402 buyer. [`Wallet::get`] fetches a URL; on a 402 it picks an offer it may pay
//! (an allowlisted `https` origin, or `http` on loopback; that origin's own `payTo` list;
//! known USDC on an allowed network; within the per-payment cap and the remaining total
//! cap), reserves the amount, signs an EIP-3009 authorisation (EIP-712, alloy) and retries
//! with `PAYMENT-SIGNATURE`. A retry after a lost answer resends the SAME proof, never a
//! new one: one 402, at most one payment.
//!
//! Once a proof has left the process no answer from the seller releases its reservation:
//! anyone holding the proof can settle it. The reservation stays [`Unresolved`] (with its
//! proof, so [`Wallet::resume`] or the next [`Wallet::get`] of that URL resends it instead
//! of paying again) until [`Wallet::reconcile`] sees, at a finalized block, that the
//! authorisation expired unused: the block's timestamp is past `validBefore` and
//! `authorizationState(from, nonce)` is still false. A proof that never connected (no byte
//! sent) gives its reservation back at once.
//!
//! While the seller is still working on a payment (`409 payment_in_flight`, `503`, a
//! `504 handler_timeout`, or no answer) the wallet backs off and resends the same proof
//! until `validBefore`.

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
    /// Bound: resends of the same proof while its outcome is pending (also bounded by the
    /// proof's `validBefore`).
    pub retries: u32,
    /// First wait between resends; it doubles up to [`MAX_BACKOFF`].
    pub backoff: Duration,
    /// Bound: proofs sent whose outcome is not yet known on chain.
    pub max_unresolved: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // Longer than a seller's default verify + settle + handler deadlines (5 + 20 + 30 s).
            request_timeout: Duration::from_secs(75),
            max_body_bytes: 1024 * 1024,
            retries: 30,
            backoff: Duration::from_millis(250),
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

/// The longest wait between resends of one proof.
pub const MAX_BACKOFF: Duration = Duration::from_secs(4);

/// An encoded payment proof: a bearer instrument, so it never prints.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedProof(String);

impl std::fmt::Debug for SignedProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignedProof(..)")
    }
}

/// A proof that left the process; its amount stays committed until the chain says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    pub network: Caip2,
    pub asset: Address,
    pub from: Address,
    pub nonce: B256,
    /// The signed `validBefore`.
    pub valid_before: u64,
    pub amount: U256,
    /// The URL it pays for.
    pub url: String,
    pub proof: SignedProof,
    /// Its outcome is still pending at the seller (no answer, in flight): resending it may
    /// still be served. False once the seller refused it.
    pub resumable: bool,
}

/// `authorizationState(from, nonce)` read at one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorizationStatus {
    pub used: bool,
    pub block_number: u64,
    /// That block's timestamp: chain time, not this machine's.
    pub block_timestamp: u64,
}

/// `authorizationState(from, nonce)` on the USDC contract `asset` of `network`, read at
/// the `finalized` (or at least `safe`) block tag, never `latest`: a release decided on
/// it must not be undone by a reorg.
pub trait AuthorizationState: Send + Sync {
    fn authorization_state<'a>(
        &'a self,
        network: &'a Caip2,
        asset: Address,
        from: Address,
        nonce: B256,
    ) -> Pin<Box<dyn Future<Output = Result<AuthorizationStatus, String>> + Send + 'a>>;
}

/// What one [`Wallet::reconcile`] pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Expired unused: the reservation left the cap.
    pub released: usize,
    /// Used on chain: the amount stays spent.
    pub spent: usize,
    /// Not yet expired at the chain's finalized block, or the chain did not answer: still
    /// reserved.
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
    /// No connection was made, so nothing was sent; a payment's reservation went back.
    #[error("could not connect: {0}")]
    Connect(String),
    #[error("no unresolved payment with that nonce, or it can no longer be served")]
    NotResumable,
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
        // Plain http only to a literal loopback address: a name (even `localhost`) goes
        // through a resolver this process does not control.
        let loopback = bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
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
    /// Release is decided on chain time alone: the finalized block's timestamp past
    /// `validBefore` with the authorisation unused. The local clock only skips entries
    /// that cannot have expired yet. Entries the chain does not answer for stay reserved.
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
            let Ok(Ok(status)) = state else {
                done.waiting += 1;
                continue;
            };
            if !status.used && status.block_timestamp <= u.valid_before {
                // Still live at the chain's own time: the seller may yet settle it.
                done.waiting += 1;
                continue;
            }
            if !self.forget(u.nonce) {
                continue;
            }
            if status.used {
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

    /// The live, resumable proof already sent for `url`, if any.
    fn pending_for(&self, url: &str) -> Option<Unresolved> {
        let now = self.clock.now();
        self.unresolved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|u| u.resumable && u.url == url && now < u.valid_before)
            .cloned()
    }

    fn set_resumable(&self, nonce: B256, resumable: bool) {
        let mut list = self
            .unresolved
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(u) = list.iter_mut().find(|u| u.nonce == nonce) {
            u.resumable = resumable;
        }
    }

    /// Resends an [`Unresolved`] proof (same nonce, never a new payment) while it is live.
    pub async fn resume(&self, nonce: B256) -> Result<Receipt, WalletError> {
        let now = self.clock.now();
        let u = self
            .unresolved()
            .into_iter()
            .find(|u| u.nonce == nonce && u.resumable && now < u.valid_before)
            .ok_or(WalletError::NotResumable)?;
        let url = reqwest::Url::parse(&u.url).map_err(|_| WalletError::Url)?;
        self.allowed(&url)?;
        self.deliver(&url, &u, true).await
    }

    /// GET `url`, paying its 402 if policy allows. A live proof already sent for `url`
    /// is resent instead of paying again.
    pub async fn get(&self, url: &str) -> Result<Receipt, WalletError> {
        let url = reqwest::Url::parse(url).map_err(|_| WalletError::Url)?;
        let origin = self.allowed(&url)?;
        if let Some(u) = self.pending_for(url.as_str()) {
            return self.deliver(&url, &u, true).await;
        }
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
        let proof = exact::pay(
            self.signer.as_ref(),
            self.address,
            offer,
            Some(required.resource.clone()),
            now,
            nonce,
        )
        .and_then(|p| encode_header(&p).map_err(|e| SignError::Signer(e.to_string())))
        .map_err(WalletError::from);
        let pending = proof.and_then(|proof| {
            let u = Unresolved {
                network: offer.network.clone(),
                asset,
                from: self.address,
                nonce,
                valid_before: now.saturating_add(offer.max_timeout_seconds),
                amount,
                url: url.as_str().to_owned(),
                proof: SignedProof(proof),
                resumable: true,
            };
            self.track(u.clone()).map(|()| u)
        });
        let pending = match pending {
            Ok(u) => u,
            Err(e) => {
                // Nothing left the process: the reservation goes back.
                self.release(amount);
                return Err(e);
            }
        };
        tracing::info!(amount = %amount, network = %offer.network, "x402 paying");
        self.deliver(&url, &pending, false).await
    }

    /// Sends `u`'s proof until the seller decides, backing off while it is pending.
    async fn deliver(
        &self,
        url: &reqwest::Url,
        u: &Unresolved,
        resent: bool,
    ) -> Result<Receipt, WalletError> {
        let mut last = None;
        let mut wait = self.limits.backoff;
        // Whether any byte of the proof may have left (a resumed proof already did).
        let mut sent = resent;
        let mut connect_error = None;
        // Each resend waits longer for its answer (a handler slower than the first
        // deadline is still served), never past what is left of the proof's validity.
        let mut deadline = self.limits.request_timeout;
        for attempt in 0..=self.limits.retries {
            if attempt > 0 {
                let now = self.clock.now();
                if now >= u.valid_before {
                    break;
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(MAX_BACKOFF);
                let left = Duration::from_secs(u.valid_before - now);
                deadline = (deadline * 2).min(left.max(self.limits.request_timeout));
            }
            let answer = self.send_within(url, Some(&u.proof.0), deadline).await;
            let (status, headers, body) = match answer {
                Ok(r) => r,
                Err(WalletError::Connect(e)) => {
                    connect_error = Some(e);
                    continue;
                }
                Err(WalletError::Http(_)) => {
                    sent = true;
                    last = None;
                    continue;
                }
                Err(e) => return Err(e),
            };
            sent = true;
            let settlement: Option<SettleResponse> = headers
                .get(PAYMENT_RESPONSE_HEADER)
                .and_then(|h| decode_header(h.as_bytes()).ok());
            if status.is_success() {
                self.forget(u.nonce);
                return Ok(Receipt {
                    status: status.as_u16(),
                    body,
                    paid: Some(u.amount),
                    settlement,
                });
            }
            if status == StatusCode::CONFLICT && settlement.as_ref().is_some_and(|s| s.success) {
                self.set_resumable(u.nonce, false);
                return Err(WalletError::PaidNotDelivered(settlement.map(Box::new)));
            }
            if status == StatusCode::PAYMENT_REQUIRED {
                self.set_resumable(u.nonce, false);
                let why = headers
                    .get(PAYMENT_REQUIRED_HEADER)
                    .and_then(|h| decode_header::<PaymentRequired>(h.as_bytes()).ok())
                    .and_then(|r| r.error);
                return Err(WalletError::Refused(reason_code(why.as_deref())));
            }
            last = Some(status.as_u16());
            if !pending(status, &body) {
                self.set_resumable(u.nonce, false);
                break;
            }
        }
        if !sent {
            // No connection was ever made: the proof never left this process.
            self.forget(u.nonce);
            self.release(u.amount);
            return Err(WalletError::Connect(connect_error.unwrap_or_default()));
        }
        Err(WalletError::Unsettled(last))
    }

    async fn send(
        &self,
        url: &reqwest::Url,
        proof: Option<&str>,
    ) -> Result<(StatusCode, reqwest::header::HeaderMap, Vec<u8>), WalletError> {
        self.send_within(url, proof, self.limits.request_timeout)
            .await
    }

    async fn send_within(
        &self,
        url: &reqwest::Url,
        proof: Option<&str>,
        deadline: Duration,
    ) -> Result<(StatusCode, reqwest::header::HeaderMap, Vec<u8>), WalletError> {
        let mut req = self.http.get(url.clone()).timeout(deadline);
        if let Some(p) = proof {
            req = req.header(PAYMENT_SIGNATURE_HEADER, p);
        }
        let mut resp = req.send().await.map_err(|e| {
            if e.is_connect() {
                WalletError::Connect(e.without_url().to_string())
            } else {
                WalletError::Http(e.without_url().to_string())
            }
        })?;
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

/// Whether the seller is still working on the payment, so the same proof may be served
/// later: `409 payment_in_flight`, `503` (busy, or the facilitator unreachable before
/// anything moved), `504 handler_timeout`. Anything else is the seller's decision.
fn pending(status: StatusCode, body: &[u8]) -> bool {
    let error = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned));
    match status {
        StatusCode::SERVICE_UNAVAILABLE => true,
        StatusCode::CONFLICT => error.as_deref() == Some("payment_in_flight"),
        StatusCode::GATEWAY_TIMEOUT => error.as_deref() == Some("handler_timeout"),
        _ => false,
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
