//! Nonce claims: the seller's replay guard. A payment is claimed before anything is spent
//! on it, and a claim leaves only by a definite "this did not settle", or by expiry.

use std::collections::{BTreeSet, HashMap};
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

/// Whether a settled payment's resource reached the buyer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The handler is running (or about to) for this payment.
    Serving,
    /// A successful response was handed to the connection.
    Delivered,
    /// Paid, but the handler failed or the buyer was gone: a retry of the same proof is
    /// served again, and the claim is listed for disputes until it is.
    NotDelivered,
}

/// What [`NonceStore::held`] lists for a paid-but-not-delivered claim.
pub const PAID_NOT_DELIVERED: &str = "paid_but_not_delivered";
/// What [`NonceStore::held`] lists for a claim whose task never reported back.
pub const EXPIRED_IN_FLIGHT: &str = "expired_in_flight";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The claim is ours.
    Fresh,
    /// Someone claimed it first.
    Taken(ClaimState),
    /// The store is at capacity with nothing it may drop.
    Full,
}

pub trait NonceStore: Send + Sync + 'static {
    /// Atomically claims `key` until unix time `expires_at` (the authorisation's
    /// `validBefore`; after it the authorisation cannot settle anywhere).
    fn claim(&self, key: NonceKey, expires_at: u64) -> BoxFuture<'_, Claim>;
    /// Settled on chain; the handler runs next ([`Delivery::Serving`]).
    fn settled(&self, key: NonceKey, transaction: String) -> BoxFuture<'_, ()>;
    /// The handler's outcome for a settled claim.
    fn delivered(&self, key: NonceKey, delivered: bool) -> BoxFuture<'_, ()>;
    /// Atomically takes a [`Delivery::NotDelivered`] claim back to `Serving` for one
    /// more attempt; its transaction, or `None` if it is not waiting for one.
    fn redeliver(&self, key: NonceKey) -> BoxFuture<'_, Option<String>>;
    fn hold(&self, key: NonceKey, reason: &'static str) -> BoxFuture<'_, ()>;
    /// Only on a definite "not settled": verify refused, or settle refused before broadcast.
    fn release(&self, key: NonceKey) -> BoxFuture<'_, ()>;
    /// Every claim that needs a reconciler or a dispute: held, paid but not delivered,
    /// and expired ones whose outcome never became definite.
    fn held(&self) -> BoxFuture<'_, Vec<(NonceKey, &'static str)>>;
}

#[derive(Debug)]
struct Entry {
    state: ClaimState,
    expires_at: u64,
}

#[derive(Debug, Default)]
struct Inner {
    entries: HashMap<NonceKey, Entry>,
    /// `entries` by expiry, so expired claims leave in O(expired log n).
    by_expiry: BTreeSet<(u64, NonceKey)>,
    /// Expired claims whose outcome is not definite, for the reconciler.
    reconcile: HashMap<NonceKey, &'static str>,
}

/// A bounded in-memory store for one seller process. Claims leave the hot map once their
/// authorisation has expired (an expired authorisation fails the offline check before it
/// reaches the store, so forgetting it cannot let it replay): delivered ones are dropped,
/// any other moves to a bounded reconciliation list. A full store refuses new claims (the
/// layer answers 503); a full reconciliation list keeps expired claims hot rather than
/// lose them.
pub struct MemoryNonceStore {
    capacity: usize,
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
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
            clock,
            inner: Mutex::new(Inner::default()),
        }
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

    /// The reconciler resolved an expired claim: it leaves the list.
    pub fn resolve(&self, key: &NonceKey) -> bool {
        self.inner().reconcile.remove(key).is_some()
    }

    fn sweep(&self, inner: &mut Inner) {
        let now = self.clock.now();
        while let Some(&(at, key)) = inner.by_expiry.first() {
            if at >= now {
                break;
            }
            let state = inner.entries.get(&key).map(|e| e.state.clone());
            let listed = match state {
                Some(ClaimState::Settled {
                    delivery: Delivery::Delivered,
                    ..
                })
                | None => None,
                Some(ClaimState::Settled { .. }) => Some(PAID_NOT_DELIVERED),
                Some(ClaimState::Held { reason }) => Some(reason),
                Some(ClaimState::InFlight) => Some(EXPIRED_IN_FLIGHT),
            };
            if let Some(reason) = listed {
                if inner.reconcile.len() >= self.capacity {
                    break; // LIMIT(reconcile-list)
                }
                inner.reconcile.insert(key, reason);
            }
            inner.by_expiry.pop_first();
            inner.entries.remove(&key);
        }
    }

    fn claim_now(&self, key: NonceKey, expires_at: u64) -> Claim {
        let mut inner = self.inner();
        self.sweep(&mut inner);
        if let Some(e) = inner.entries.get(&key) {
            return Claim::Taken(e.state.clone());
        }
        if let Some(&reason) = inner.reconcile.get(&key) {
            return Claim::Taken(ClaimState::Held { reason });
        }
        if inner.entries.len() >= self.capacity {
            return Claim::Full; // LIMIT(nonce-store)
        }
        inner.entries.insert(
            key,
            Entry {
                state: ClaimState::InFlight,
                expires_at,
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
            e.state = state;
            return;
        }
        let late = match state {
            ClaimState::Settled {
                delivery: Delivery::Delivered,
                ..
            }
            | ClaimState::InFlight => return,
            ClaimState::Settled { .. } => PAID_NOT_DELIVERED,
            ClaimState::Held { reason } => reason,
        };
        if let Some(listed) = inner.reconcile.get_mut(&key) {
            *listed = late;
        } else if inner.reconcile.len() < self.capacity {
            inner.reconcile.insert(key, late);
        } else {
            tracing::error!(payer = %key.payer, "x402 reconciliation list full: late outcome dropped");
        }
    }

    fn remove(&self, key: NonceKey) {
        let mut inner = self.inner();
        if let Some(e) = inner.entries.remove(&key) {
            inner.by_expiry.remove(&(e.expires_at, key));
        }
    }
}

impl NonceStore for MemoryNonceStore {
    fn claim(&self, key: NonceKey, expires_at: u64) -> BoxFuture<'_, Claim> {
        let c = self.claim_now(key, expires_at);
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
        if let Some(ClaimState::Settled { transaction, .. }) = state {
            let delivery = if delivered {
                Delivery::Delivered
            } else {
                Delivery::NotDelivered
            };
            self.set(
                key,
                ClaimState::Settled {
                    transaction,
                    delivery,
                },
            );
        } else if !delivered && state.is_none() {
            self.set(
                key,
                ClaimState::Settled {
                    transaction: String::new(),
                    delivery: Delivery::NotDelivered,
                },
            );
        } else if delivered && state.is_none() {
            self.inner().reconcile.remove(&key);
        }
        Box::pin(async {})
    }

    fn redeliver(&self, key: NonceKey) -> BoxFuture<'_, Option<String>> {
        let mut inner = self.inner();
        let tx = match inner.entries.get_mut(&key) {
            Some(Entry {
                state:
                    ClaimState::Settled {
                        transaction,
                        delivery: delivery @ Delivery::NotDelivered,
                    },
                ..
            }) => {
                *delivery = Delivery::Serving;
                Some(transaction.clone())
            }
            _ => None,
        };
        drop(inner);
        Box::pin(async move { tx })
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
            .chain(inner.reconcile.iter().map(|(k, r)| (*k, *r)))
            .collect();
        drop(inner);
        held.sort_unstable();
        Box::pin(async move { held })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xpay_x402::clock::ManualClock;

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
        assert_eq!(s.claim(key(1), 200).await, Claim::Fresh);
        assert_eq!(
            s.claim(key(1), 200).await,
            Claim::Taken(ClaimState::InFlight)
        );
        s.hold(key(1), "settle_timeout").await;
        s.release(key(1)).await;
        assert_eq!(
            s.claim(key(1), 200).await,
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
        assert_eq!(s.claim(key(1), 150).await, Claim::Fresh);
        s.settled(key(1), "0xabc".into()).await;
        s.delivered(key(1), true).await;
        assert_eq!(s.claim(key(2), 150).await, Claim::Fresh);
        s.hold(key(2), "settle_timeout").await;
        assert_eq!(
            s.claim(key(3), 150).await,
            Claim::Full,
            "nothing expired yet"
        );
        clock.set(151);
        assert_eq!(s.claim(key(3), 300).await, Claim::Fresh);
        assert_eq!(s.claim(key(4), 300).await, Claim::Fresh);
        assert_eq!(s.claim(key(5), 300).await, Claim::Full);
        assert_eq!(s.held().await, vec![(key(2), "settle_timeout")]);
        assert_eq!(
            s.claim(key(2), 300).await,
            Claim::Taken(ClaimState::Held {
                reason: "settle_timeout"
            }),
            "a listed claim still refuses its nonce"
        );
        assert!(s.resolve(&key(2)));
        assert_eq!(s.held().await.len(), 0);
    }

    #[tokio::test]
    async fn an_undelivered_payment_is_redelivered_one_attempt_at_a_time() {
        let s = MemoryNonceStore::new(8, Arc::new(ManualClock::at(100)));
        assert_eq!(s.claim(key(1), 150).await, Claim::Fresh);
        assert_eq!(s.redeliver(key(1)).await, None, "not settled");
        s.settled(key(1), "0xabc".into()).await;
        assert_eq!(s.redeliver(key(1)).await, None, "still serving");
        s.delivered(key(1), false).await;
        assert_eq!(s.held().await, vec![(key(1), PAID_NOT_DELIVERED)]);
        assert_eq!(s.redeliver(key(1)).await, Some("0xabc".into()));
        assert_eq!(s.redeliver(key(1)).await, None, "one attempt at a time");
        s.delivered(key(1), true).await;
        assert_eq!(s.held().await.len(), 0);
        assert_eq!(s.redeliver(key(1)).await, None, "delivered");
    }

    #[tokio::test]
    async fn a_late_outcome_for_an_expired_claim_is_still_recorded() {
        let clock = ManualClock::at(100);
        let s = MemoryNonceStore::new(1, Arc::new(clock.clone()));
        assert_eq!(s.claim(key(1), 150).await, Claim::Fresh);
        clock.set(151);
        assert_eq!(s.claim(key(2), 300).await, Claim::Fresh);
        assert_eq!(s.held().await, vec![(key(1), EXPIRED_IN_FLIGHT)]);
        s.settled(key(1), "0xabc".into()).await;
        assert_eq!(s.held().await, vec![(key(1), PAID_NOT_DELIVERED)]);
        s.delivered(key(1), true).await;
        assert_eq!(s.held().await.len(), 0);
    }

    #[tokio::test]
    async fn a_full_reconciliation_list_keeps_expired_claims_hot() {
        let clock = ManualClock::at(100);
        let s = MemoryNonceStore::new(1, Arc::new(clock.clone()));
        assert_eq!(s.claim(key(1), 150).await, Claim::Fresh);
        s.hold(key(1), "settle_timeout").await;
        clock.set(151);
        assert_eq!(s.claim(key(2), 160).await, Claim::Fresh);
        s.hold(key(2), "settle_timeout").await;
        clock.set(161);
        assert_eq!(s.claim(key(3), 300).await, Claim::Full);
        assert_eq!(s.held().await.len(), 2, "nothing lost");
    }

    #[tokio::test]
    async fn expired_held_and_in_flight_claims_leave_the_hot_store() {
        let clock = ManualClock::at(100);
        let s = MemoryNonceStore::new(2, Arc::new(clock.clone()));
        assert_eq!(s.claim(key(1), 150).await, Claim::Fresh);
        s.hold(key(1), "settle_timeout").await;
        assert_eq!(s.claim(key(2), 150).await, Claim::Fresh);
        clock.set(151);
        assert_eq!(s.claim(key(3), 300).await, Claim::Fresh);
        assert_eq!(s.state(&key(2)), None, "in-flight past expiry left");
        assert_eq!(s.held().await.len(), 2, "both kept for reconciliation");
    }
}
