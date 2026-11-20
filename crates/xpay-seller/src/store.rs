//! Nonce claims: the seller's replay guard. A payment is claimed before anything is spent
//! on it, and a claim leaves only by a definite "this did not settle", or by expiry.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use alloy_primitives::{Address, B256};
use xpay_x402::clock::Clock;
use xpay_x402::facilitator::BoxFuture;

/// EIP-3009 nonces are scoped to (token contract, authoriser).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NonceKey {
    pub asset: Address,
    pub payer: Address,
    pub nonce: B256,
}

/// The resource a payment was claimed for (a digest of method and path): a payment that
/// was not delivered is served again only to the same resource.
pub type Route = B256;

/// Whether a settled payment's resource reached the buyer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The handler is running (or about to) for this payment.
    Serving,
    /// A response was handed to the connection.
    Delivered,
    /// Paid, but the handler failed (timeout, 5xx, panic) or the buyer was gone: a retry
    /// of the same proof for the same resource is served again, a bounded number of
    /// times, and the claim is listed for disputes until it is.
    NotDelivered,
}

/// What [`NonceStore::held`] lists for a paid-but-not-delivered claim.
pub const PAID_NOT_DELIVERED: &str = "paid_but_not_delivered";
/// What [`NonceStore::held`] lists for a claim whose task never reported back.
pub const EXPIRED_IN_FLIGHT: &str = "expired_in_flight";

/// How many times one paid-but-not-delivered payment is served again.
pub const MAX_REDELIVERIES: u8 = 3;
/// Default bound on one payer's undecided claims (held, paid but not delivered, listed).
pub const MAX_UNDECIDED_PER_PAYER: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimState {
    /// Claimed; verify/settle under way.
    InFlight,
    /// Settled on chain.
    Settled {
        transaction: String,
        delivery: Delivery,
    },
    /// Settlement outcome unknown: kept for reconciliation, never served, never released.
    Held { reason: &'static str },
}

impl ClaimState {
    /// The outcome needs a reconciler or a dispute.
    fn undecided(&self) -> bool {
        matches!(
            self,
            Self::Held { .. }
                | Self::Settled {
                    delivery: Delivery::NotDelivered,
                    ..
                }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The claim is ours.
    Fresh,
    /// Someone claimed it first.
    Taken(ClaimState),
    /// The store is at capacity with nothing it may drop.
    Full,
    /// This payer already has as many undecided claims as one payer may.
    PayerBusy,
}

/// The answer to [`NonceStore::redeliver`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redelivery {
    /// Serve it again; the settlement's transaction.
    Serve(String),
    /// The payment was made for another resource.
    OtherResource,
    /// Not waiting for a delivery (in flight, delivered, or out of attempts).
    No,
}

/// One undecided claim that left the hot store, for the dispute log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listed {
    pub key: NonceKey,
    pub reason: &'static str,
}

pub trait NonceStore: Send + Sync + 'static {
    /// Atomically claims `key` for `route` until unix time `expires_at` (the
    /// authorisation's `validBefore`; after it the authorisation cannot settle anywhere).
    fn claim(&self, key: NonceKey, route: Route, expires_at: u64) -> BoxFuture<'_, Claim>;
    /// Settled on chain; the handler runs next ([`Delivery::Serving`]).
    fn settled(&self, key: NonceKey, transaction: String) -> BoxFuture<'_, ()>;
    /// The handler's outcome for a settled claim.
    fn delivered(&self, key: NonceKey, delivered: bool) -> BoxFuture<'_, ()>;
    /// Atomically takes a [`Delivery::NotDelivered`] claim for `route` back to `Serving`
    /// for one more attempt.
    fn redeliver(&self, key: NonceKey, route: Route) -> BoxFuture<'_, Redelivery>;
    fn hold(&self, key: NonceKey, reason: &'static str) -> BoxFuture<'_, ()>;
    /// Only on a definite "not settled": verify refused, settle refused before broadcast,
    /// or the task ended before anything was sent to settle.
    fn release(&self, key: NonceKey) -> BoxFuture<'_, ()>;
    /// Every claim that needs a reconciler or a dispute: held, paid but not delivered,
    /// and expired ones whose outcome never became definite.
    fn held(&self) -> BoxFuture<'_, Vec<(NonceKey, &'static str)>>;
    /// Up to `max` of the oldest expired undecided claims, still listed.
    fn listed(&self, max: usize) -> BoxFuture<'_, Vec<Listed>>;
    /// The dispute log has `done`: they leave the list (unless their outcome changed since).
    fn resolve(&self, done: Vec<Listed>) -> BoxFuture<'_, usize>;
}

/// A durable record of undecided claims, written before the store forgets them.
pub trait DisputeLog: Send + Sync {
    fn record<'a>(&'a self, batch: &'a [Listed]) -> BoxFuture<'a, Result<(), String>>;
}

/// Moves up to `max` expired undecided claims from `store` to `log`. A claim leaves the
/// store only after the log took it; a log that fails or misses `deadline` keeps them.
pub async fn drain_to_log(
    store: &dyn NonceStore,
    log: &dyn DisputeLog,
    max: usize,
    deadline: std::time::Duration,
) -> Result<usize, String> {
    let batch = store.listed(max).await;
    if batch.is_empty() {
        return Ok(0);
    }
    match tokio::time::timeout(deadline, log.record(&batch)).await {
        Ok(Ok(())) => Ok(store.resolve(batch).await),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("dispute log timed out".into()),
    }
}

/// Appends one JSON line per claim to a file, synced before it returns.
#[derive(Debug, Clone)]
pub struct FileDisputeLog {
    path: std::path::PathBuf,
}

impl FileDisputeLog {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl DisputeLog for FileDisputeLog {
    fn record<'a>(&'a self, batch: &'a [Listed]) -> BoxFuture<'a, Result<(), String>> {
        use std::fmt::Write as _;
        let mut lines = String::new();
        for l in batch {
            // Fixed-shape fields only: addresses, a nonce and a static reason code.
            let _ = writeln!(
                lines,
                r#"{{"asset":"{}","payer":"{}","nonce":"{}","reason":"{}"}}"#,
                l.key.asset, l.key.payer, l.key.nonce, l.reason
            );
        }
        let path = self.path.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                use std::io::Write as _;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?;
                f.write_all(lines.as_bytes())?;
                f.sync_data()
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.kind().to_string())
        })
    }
}

#[derive(Debug)]
struct Entry {
    state: ClaimState,
    route: Route,
    expires_at: u64,
    redeliveries: u8,
}

#[derive(Debug, Default)]
struct Inner {
    entries: HashMap<NonceKey, Entry>,
    /// `entries` by expiry, so expired claims leave in O(expired log n).
    by_expiry: BTreeSet<(u64, NonceKey)>,
    /// Expired claims whose outcome is not definite, for the dispute log: key → (order,
    /// reason), and the order → key index that drops the oldest when full.
    reconcile: HashMap<NonceKey, (u64, &'static str)>,
    reconcile_order: BTreeMap<u64, NonceKey>,
    next_order: u64,
    /// Undecided claims per payer, hot or listed.
    undecided: HashMap<Address, usize>,
}

impl Inner {
    fn count(&mut self, payer: Address, was: bool, now: bool) {
        match (was, now) {
            (false, true) => *self.undecided.entry(payer).or_default() += 1,
            (true, false) => {
                if let Some(n) = self.undecided.get_mut(&payer) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        self.undecided.remove(&payer);
                    }
                }
            }
            _ => {}
        }
    }

    fn unlist(&mut self, key: &NonceKey) -> Option<&'static str> {
        let (order, reason) = self.reconcile.remove(key)?;
        self.reconcile_order.remove(&order);
        self.count(key.payer, true, false);
        Some(reason)
    }
}

/// A bounded in-memory store for one seller process. Claims leave the hot map once their
/// authorisation has expired (an expired authorisation fails the offline check before it
/// reaches the store, so forgetting it cannot let it replay): delivered ones are dropped,
/// any other moves to a bounded reconciliation list, drained by [`drain_to_log`]. A full
/// list drops its oldest entry to the error log (target `x402_dispute`) and counts it in
/// [`MemoryNonceStore::dropped`]; it never blocks the sweep, so it cannot lock buyers out.
/// A full store refuses new claims (the layer answers 503), and one payer's undecided
/// claims are bounded.
pub struct MemoryNonceStore {
    capacity: usize,
    per_payer: usize,
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
    dropped: AtomicU64,
}

impl std::fmt::Debug for MemoryNonceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryNonceStore")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl MemoryNonceStore {
    pub fn new(capacity: usize, clock: Arc<dyn Clock>) -> Self {
        Self {
            capacity,
            per_payer: MAX_UNDECIDED_PER_PAYER,
            clock,
            inner: Mutex::new(Inner::default()),
            dropped: AtomicU64::new(0),
        }
    }

    /// Bounds one payer's undecided claims (default [`MAX_UNDECIDED_PER_PAYER`]).
    #[must_use]
    pub fn with_max_undecided_per_payer(mut self, n: usize) -> Self {
        self.per_payer = n;
        self
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn state(&self, key: &NonceKey) -> Option<ClaimState> {
        self.inner().entries.get(key).map(|e| e.state.clone())
    }

    /// Claims in the hot map.
    pub fn len(&self) -> usize {
        self.inner().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Undecided claims dropped from a full reconciliation list (each logged first).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Lists an expired undecided claim, dropping the oldest to the log when full.
    fn list(&self, inner: &mut Inner, key: NonceKey, reason: &'static str) {
        if let Some((_, listed)) = inner.reconcile.get_mut(&key) {
            *listed = reason;
            return;
        }
        while inner.reconcile.len() >= self.capacity.max(1) {
            // LIMIT(reconcile-list)
            let Some((_, oldest)) = inner.reconcile_order.pop_first() else {
                break;
            };
            if let Some((_, r)) = inner.reconcile.remove(&oldest) {
                inner.count(oldest.payer, true, false);
                self.dropped.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    target: "x402_dispute",
                    asset = %oldest.asset,
                    payer = %oldest.payer,
                    nonce = %oldest.nonce,
                    reason = r,
                    "x402 reconciliation list full: oldest undecided claim dropped"
                );
            }
        }
        let order = inner.next_order;
        inner.next_order += 1;
        inner.reconcile.insert(key, (order, reason));
        inner.reconcile_order.insert(order, key);
        inner.count(key.payer, false, true);
    }

    fn sweep(&self, inner: &mut Inner) {
        let now = self.clock.now();
        while let Some(&(at, key)) = inner.by_expiry.first() {
            if at >= now {
                break;
            }
            inner.by_expiry.pop_first();
            let Some(e) = inner.entries.remove(&key) else {
                continue;
            };
            let was = e.state.undecided();
            inner.count(key.payer, was, false);
            let listed = match e.state {
                ClaimState::Settled {
                    delivery: Delivery::Delivered,
                    ..
                } => None,
                ClaimState::Settled { .. } => Some(PAID_NOT_DELIVERED),
                ClaimState::Held { reason } => Some(reason),
                ClaimState::InFlight => Some(EXPIRED_IN_FLIGHT),
            };
            if let Some(reason) = listed {
                self.list(inner, key, reason);
            }
        }
    }

    fn claim_now(&self, key: NonceKey, route: Route, expires_at: u64) -> Claim {
        let mut inner = self.inner();
        self.sweep(&mut inner);
        if let Some(e) = inner.entries.get(&key) {
            return Claim::Taken(e.state.clone());
        }
        if let Some(&(_, reason)) = inner.reconcile.get(&key) {
            return Claim::Taken(ClaimState::Held { reason });
        }
        if inner.entries.len() >= self.capacity {
            return Claim::Full; // LIMIT(nonce-store)
        }
        if inner.undecided.get(&key.payer).copied().unwrap_or(0) >= self.per_payer {
            return Claim::PayerBusy; // LIMIT(payer-undecided)
        }
        inner.entries.insert(
            key,
            Entry {
                state: ClaimState::InFlight,
                route,
                expires_at,
                redeliveries: 0,
            },
        );
        inner.by_expiry.insert((expires_at, key));
        Claim::Fresh
    }

    /// Sets the state of a live claim; a claim that already expired is recorded for the
    /// reconciler instead, unless the news is a delivery.
    fn set(&self, key: NonceKey, state: ClaimState) {
        let mut inner = self.inner();
        if let Some(e) = inner.entries.get_mut(&key) {
            let (was, now) = (e.state.undecided(), state.undecided());
            e.state = state;
            inner.count(key.payer, was, now);
            return;
        }
        let late = match state {
            ClaimState::Settled {
                delivery: Delivery::Delivered,
                ..
            } => {
                inner.unlist(&key);
                return;
            }
            ClaimState::InFlight => return,
            ClaimState::Settled { .. } => PAID_NOT_DELIVERED,
            ClaimState::Held { reason } => reason,
        };
        self.list(&mut inner, key, late);
    }

    fn remove(&self, key: NonceKey) {
        let mut inner = self.inner();
        if let Some(e) = inner.entries.remove(&key) {
            inner.by_expiry.remove(&(e.expires_at, key));
            inner.count(key.payer, e.state.undecided(), false);
        }
    }
}

impl NonceStore for MemoryNonceStore {
    fn claim(&self, key: NonceKey, route: Route, expires_at: u64) -> BoxFuture<'_, Claim> {
        let c = self.claim_now(key, route, expires_at);
        Box::pin(async move { c })
    }

    fn settled(&self, key: NonceKey, transaction: String) -> BoxFuture<'_, ()> {
        self.set(
            key,
            ClaimState::Settled {
                transaction,
                delivery: Delivery::Serving,
            },
        );
        Box::pin(async {})
    }

    fn delivered(&self, key: NonceKey, delivered: bool) -> BoxFuture<'_, ()> {
        let state = self.state(&key);
        let delivery = if delivered {
            Delivery::Delivered
        } else {
            Delivery::NotDelivered
        };
        match state {
            Some(ClaimState::Settled { transaction, .. }) => self.set(
                key,
                ClaimState::Settled {
                    transaction,
                    delivery,
                },
            ),
            None => self.set(
                key,
                ClaimState::Settled {
                    transaction: String::new(),
                    delivery,
                },
            ),
            Some(_) => {}
        }
        Box::pin(async {})
    }

    fn redeliver(&self, key: NonceKey, route: Route) -> BoxFuture<'_, Redelivery> {
        let mut inner = self.inner();
        let r = match inner.entries.get_mut(&key) {
            Some(e) if e.route != route => Redelivery::OtherResource,
            Some(Entry {
                state:
                    ClaimState::Settled {
                        transaction,
                        delivery: delivery @ Delivery::NotDelivered,
                    },
                redeliveries,
                ..
            }) if *redeliveries < MAX_REDELIVERIES => {
                // LIMIT(redeliveries)
                *redeliveries += 1;
                *delivery = Delivery::Serving;
                let tx = transaction.clone();
                inner.count(key.payer, true, false);
                Redelivery::Serve(tx)
            }
            _ => Redelivery::No,
        };
        drop(inner);
        Box::pin(async move { r })
    }

    fn hold(&self, key: NonceKey, reason: &'static str) -> BoxFuture<'_, ()> {
        self.set(key, ClaimState::Held { reason });
        Box::pin(async {})
    }

    fn release(&self, key: NonceKey) -> BoxFuture<'_, ()> {
        if matches!(self.state(&key), Some(ClaimState::InFlight)) {
            self.remove(key);
        }
        Box::pin(async {})
    }

    fn held(&self) -> BoxFuture<'_, Vec<(NonceKey, &'static str)>> {
        let inner = self.inner();
        let mut held: Vec<_> = inner
            .entries
            .iter()
            .filter_map(|(k, e)| match e.state {
                ClaimState::Held { reason } => Some((*k, reason)),
                ClaimState::Settled {
                    delivery: Delivery::NotDelivered,
                    ..
                } => Some((*k, PAID_NOT_DELIVERED)),
                _ => None,
            })
            .chain(inner.reconcile.iter().map(|(k, (_, r))| (*k, *r)))
            .collect();
        drop(inner);
        held.sort_unstable();
        Box::pin(async move { held })
    }

    fn listed(&self, max: usize) -> BoxFuture<'_, Vec<Listed>> {
        let inner = self.inner();
        let out: Vec<Listed> = inner
            .reconcile_order
            .values()
            .take(max)
            .filter_map(|k| {
                inner
                    .reconcile
                    .get(k)
                    .map(|&(_, reason)| Listed { key: *k, reason })
            })
            .collect();
        drop(inner);
        Box::pin(async move { out })
    }

    fn resolve(&self, done: Vec<Listed>) -> BoxFuture<'_, usize> {
        let mut inner = self.inner();
        let mut n = 0;
        for l in done {
            if inner.reconcile.get(&l.key).map(|&(_, r)| r) == Some(l.reason) {
                inner.unlist(&l.key);
                n += 1;
            }
        }
        drop(inner);
        Box::pin(async move { n })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xpay_x402::clock::ManualClock;

    const R: Route = B256::ZERO;

    fn key(n: u8) -> NonceKey {
        NonceKey {
            asset: Address::ZERO,
            payer: Address::repeat_byte(1),
            nonce: B256::repeat_byte(n),
        }
    }

    #[tokio::test]
    async fn a_claim_is_taken_once_and_release_frees_only_in_flight() {
        let clock = ManualClock::at(100);
        let s = MemoryNonceStore::new(8, Arc::new(clock));
        assert_eq!(s.claim(key(1), R, 200).await, Claim::Fresh);
        assert_eq!(
            s.claim(key(1), R, 200).await,
            Claim::Taken(ClaimState::InFlight)
        );
        s.hold(key(1), "settle_timeout").await;
        s.release(key(1)).await;
        assert_eq!(
            s.claim(key(1), R, 200).await,
            Claim::Taken(ClaimState::Held {
                reason: "settle_timeout"
            })
        );
        assert_eq!(s.held().await, vec![(key(1), "settle_timeout")]);
    }

    #[tokio::test]
    async fn a_full_store_sheds_only_expired_claims_and_lists_the_undecided() {
        let clock = ManualClock::at(100);
        let s = MemoryNonceStore::new(2, Arc::new(clock.clone()));
        assert_eq!(s.claim(key(1), R, 150).await, Claim::Fresh);
        s.settled(key(1), "0xabc".into()).await;
        s.delivered(key(1), true).await;
        assert_eq!(s.claim(key(2), R, 150).await, Claim::Fresh);
        s.hold(key(2), "settle_timeout").await;
        assert_eq!(
            s.claim(key(3), R, 150).await,
            Claim::Full,
            "nothing expired yet"
        );
        clock.set(151);
        assert_eq!(s.claim(key(3), R, 300).await, Claim::Fresh);
        assert_eq!(s.claim(key(4), R, 300).await, Claim::Fresh);
        assert_eq!(s.claim(key(5), R, 300).await, Claim::Full);
        assert_eq!(s.held().await, vec![(key(2), "settle_timeout")]);
        assert_eq!(
            s.claim(key(2), R, 300).await,
            Claim::Taken(ClaimState::Held {
                reason: "settle_timeout"
            }),
            "a listed claim still refuses its nonce"
        );
        let listed = s.listed(10).await;
        assert_eq!(
            listed,
            vec![Listed {
                key: key(2),
                reason: "settle_timeout"
            }]
        );
        s.hold(key(2), "settle_error").await;
        assert_eq!(s.resolve(listed).await, 0, "changed since: kept");
        assert_eq!(s.resolve(s.listed(10).await).await, 1);
        assert_eq!(s.held().await.len(), 0);
    }

    #[tokio::test]
    async fn an_undelivered_payment_is_redelivered_one_attempt_at_a_time() {
        let s = MemoryNonceStore::new(8, Arc::new(ManualClock::at(100)));
        assert_eq!(s.claim(key(1), R, 150).await, Claim::Fresh);
        assert_eq!(s.redeliver(key(1), R).await, Redelivery::No, "not settled");
        s.settled(key(1), "0xabc".into()).await;
        assert_eq!(
            s.redeliver(key(1), R).await,
            Redelivery::No,
            "still serving"
        );
        s.delivered(key(1), false).await;
        assert_eq!(s.held().await, vec![(key(1), PAID_NOT_DELIVERED)]);
        assert_eq!(
            s.redeliver(key(1), R).await,
            Redelivery::Serve("0xabc".into())
        );
        assert_eq!(
            s.redeliver(key(1), R).await,
            Redelivery::No,
            "one attempt at a time"
        );
        s.delivered(key(1), true).await;
        assert_eq!(s.held().await.len(), 0);
        assert_eq!(s.redeliver(key(1), R).await, Redelivery::No, "delivered");
    }

    #[tokio::test]
    async fn a_late_outcome_for_an_expired_claim_is_still_recorded() {
        let clock = ManualClock::at(100);
        let s = MemoryNonceStore::new(1, Arc::new(clock.clone()));
        assert_eq!(s.claim(key(1), R, 150).await, Claim::Fresh);
        clock.set(151);
        assert_eq!(s.claim(key(2), R, 300).await, Claim::Fresh);
        assert_eq!(s.held().await, vec![(key(1), EXPIRED_IN_FLIGHT)]);
        s.settled(key(1), "0xabc".into()).await;
        assert_eq!(s.held().await, vec![(key(1), PAID_NOT_DELIVERED)]);
        s.delivered(key(1), true).await;
        assert_eq!(s.held().await.len(), 0);
    }

    #[tokio::test]
    async fn a_full_reconciliation_list_drops_its_oldest_entry_and_counts_it() {
        let clock = ManualClock::at(100);
        let s = MemoryNonceStore::new(1, Arc::new(clock.clone()));
        assert_eq!(s.claim(key(1), R, 150).await, Claim::Fresh);
        s.hold(key(1), "settle_timeout").await;
        clock.set(151);
        assert_eq!(s.claim(key(2), R, 160).await, Claim::Fresh);
        s.hold(key(2), "settle_error").await;
        clock.set(161);
        assert_eq!(s.claim(key(3), R, 300).await, Claim::Fresh);
        assert_eq!(s.held().await, vec![(key(2), "settle_error")]);
        assert_eq!(s.dropped(), 1);
    }

    #[tokio::test]
    async fn one_payers_undecided_claims_are_bounded() {
        let clock = ManualClock::at(100);
        let s = MemoryNonceStore::new(64, Arc::new(clock.clone())).with_max_undecided_per_payer(2);
        for n in 0..2 {
            assert_eq!(s.claim(key(n), R, 150).await, Claim::Fresh);
            s.hold(key(n), "settle_timeout").await;
        }
        assert_eq!(s.claim(key(2), R, 150).await, Claim::PayerBusy);
        let other = NonceKey {
            payer: Address::repeat_byte(2),
            ..key(2)
        };
        assert_eq!(
            s.claim(other, R, 150).await,
            Claim::Fresh,
            "other payers unaffected"
        );
        clock.set(151);
        // Expired, the two are listed and still count until the dispute log takes them.
        assert_eq!(s.claim(key(3), R, 300).await, Claim::PayerBusy);
        assert_eq!(s.resolve(s.listed(10).await).await, 3);
        assert_eq!(s.claim(key(3), R, 300).await, Claim::Fresh);
    }

    #[tokio::test]
    async fn redelivery_is_for_the_same_resource_and_bounded() {
        let s = MemoryNonceStore::new(8, Arc::new(ManualClock::at(100)));
        let other = B256::repeat_byte(9);
        assert_eq!(s.claim(key(1), R, 150).await, Claim::Fresh);
        s.settled(key(1), "0xabc".into()).await;
        s.delivered(key(1), false).await;
        assert_eq!(s.redeliver(key(1), other).await, Redelivery::OtherResource);
        for _ in 0..MAX_REDELIVERIES {
            assert_eq!(
                s.redeliver(key(1), R).await,
                Redelivery::Serve("0xabc".into())
            );
            s.delivered(key(1), false).await;
        }
        assert_eq!(
            s.redeliver(key(1), R).await,
            Redelivery::No,
            "out of attempts"
        );
        assert_eq!(s.held().await, vec![(key(1), PAID_NOT_DELIVERED)]);
    }

    struct Log(std::sync::Mutex<Vec<Listed>>, bool);

    impl DisputeLog for Log {
        fn record<'a>(&'a self, batch: &'a [Listed]) -> BoxFuture<'a, Result<(), String>> {
            if self.1 {
                self.0.lock().unwrap().extend_from_slice(batch);
            }
            let ok = self.1;
            Box::pin(async move { if ok { Ok(()) } else { Err("disk full".into()) } })
        }
    }

    #[tokio::test]
    async fn the_dispute_log_takes_listed_claims_and_a_failed_write_keeps_them() {
        let clock = ManualClock::at(100);
        let s = MemoryNonceStore::new(8, Arc::new(clock.clone()));
        assert_eq!(s.claim(key(1), R, 150).await, Claim::Fresh);
        s.hold(key(1), "settle_timeout").await;
        clock.set(151);
        assert_eq!(s.claim(key(2), R, 300).await, Claim::Fresh);
        let d = std::time::Duration::from_secs(1);
        let broken = Log(std::sync::Mutex::default(), false);
        assert!(drain_to_log(&s, &broken, 10, d).await.is_err());
        assert_eq!(s.held().await.len(), 1, "kept");
        let log = Log(std::sync::Mutex::default(), true);
        assert_eq!(drain_to_log(&s, &log, 10, d).await, Ok(1));
        assert_eq!(log.0.lock().unwrap().len(), 1);
        assert_eq!(s.held().await.len(), 0);
    }

    #[tokio::test]
    async fn the_file_dispute_log_appends_json_lines() {
        let path = std::env::temp_dir().join(format!("xpay-disputes-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let log = FileDisputeLog::new(&path);
        let l = Listed {
            key: key(1),
            reason: "settle_timeout",
        };
        log.record(&[l]).await.unwrap();
        log.record(&[l]).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        let v: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(v["reason"], "settle_timeout");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn expired_held_and_in_flight_claims_leave_the_hot_store() {
        let clock = ManualClock::at(100);
        let s = MemoryNonceStore::new(2, Arc::new(clock.clone()));
        assert_eq!(s.claim(key(1), R, 150).await, Claim::Fresh);
        s.hold(key(1), "settle_timeout").await;
        assert_eq!(s.claim(key(2), R, 150).await, Claim::Fresh);
        clock.set(151);
        assert_eq!(s.claim(key(3), R, 300).await, Claim::Fresh);
        assert_eq!(s.state(&key(2)), None, "in-flight past expiry left");
        assert_eq!(s.held().await.len(), 2, "both kept for reconciliation");
    }

    #[tokio::test]
    async fn a_full_reconciliation_list_never_locks_out_new_claims() {
        let clock = ManualClock::at(100);
        let s = MemoryNonceStore::new(2, Arc::new(clock.clone()));
        let mut t = 100;
        for n in 0..6u8 {
            assert_eq!(s.claim(key(n), R, t + 10).await, Claim::Fresh, "claim {n}");
            s.hold(key(n), "settle_timeout").await;
            t += 11;
            clock.set(t);
        }
        // Delivered claims behind undecided ones still leave.
        assert_eq!(s.claim(key(10), R, t + 10).await, Claim::Fresh);
        s.settled(key(10), "0xabc".into()).await;
        s.delivered(key(10), true).await;
        assert_eq!(s.claim(key(11), R, t + 10).await, Claim::Fresh);
        t += 11;
        clock.set(t);
        assert_eq!(s.claim(key(12), R, t + 10).await, Claim::Fresh);
        assert_eq!(s.claim(key(13), R, t + 10).await, Claim::Fresh);
    }
}
