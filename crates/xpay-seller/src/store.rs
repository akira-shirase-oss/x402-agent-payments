//! Nonce claims: the seller's replay guard. A payment is claimed before anything is spent
//! on it, and a claim leaves only by a definite "this did not settle".

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use alloy_primitives::{Address, B256};
use xpay_x402::clock::Clock;
use xpay_x402::facilitator::BoxFuture;

/// EIP-3009 nonces are scoped to (token contract, authoriser).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NonceKey {
    pub asset: Address,
    pub payer: Address,
    pub nonce: B256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimState {
    /// Claimed; verify/settle under way.
    InFlight,
    /// Settled on chain; served once.
    Settled { transaction: String },
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
    fn settled(&self, key: NonceKey, transaction: String) -> BoxFuture<'_, ()>;
    fn hold(&self, key: NonceKey, reason: &'static str) -> BoxFuture<'_, ()>;
    /// Only on a definite "not settled": verify refused, or settle refused.
    fn release(&self, key: NonceKey) -> BoxFuture<'_, ()>;
    /// Every held claim, for the reconciler.
    fn held(&self) -> BoxFuture<'_, Vec<(NonceKey, &'static str)>>;
}

#[derive(Debug)]
struct Entry {
    state: ClaimState,
    expires_at: u64,
}

/// A bounded in-memory store. When full it drops only settled claims
/// whose authorisation has expired (an expired authorisation fails the offline check before
/// it reaches the store, so forgetting it cannot let it replay); in-flight and held claims
/// are never dropped, and a full store refuses new claims (the layer answers 503).
pub struct MemoryNonceStore {
    capacity: usize,
    clock: Arc<dyn Clock>,
    entries: Mutex<HashMap<NonceKey, Entry>>,
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
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<NonceKey, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn state(&self, key: &NonceKey) -> Option<ClaimState> {
        self.entries().get(key).map(|e| e.state.clone())
    }

    pub fn len(&self) -> usize {
        self.entries().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn claim_now(&self, key: NonceKey, expires_at: u64) -> Claim {
        let mut entries = self.entries();
        if let Some(e) = entries.get(&key) {
            return Claim::Taken(e.state.clone());
        }
        if entries.len() >= self.capacity {
            let now = self.clock.now();
            entries.retain(|_, e| {
                !(matches!(e.state, ClaimState::Settled { .. }) && e.expires_at < now)
            });
            if entries.len() >= self.capacity {
                return Claim::Full; // LIMIT(nonce-store)
            }
        }
        entries.insert(
            key,
            Entry {
                state: ClaimState::InFlight,
                expires_at,
            },
        );
        Claim::Fresh
    }

    fn set(&self, key: NonceKey, state: ClaimState) {
        if let Some(e) = self.entries().get_mut(&key) {
            e.state = state;
        }
    }
}

impl NonceStore for MemoryNonceStore {
    fn claim(&self, key: NonceKey, expires_at: u64) -> BoxFuture<'_, Claim> {
        let c = self.claim_now(key, expires_at);
        Box::pin(async move { c })
    }

    fn settled(&self, key: NonceKey, transaction: String) -> BoxFuture<'_, ()> {
        self.set(key, ClaimState::Settled { transaction });
        Box::pin(async {})
    }

    fn hold(&self, key: NonceKey, reason: &'static str) -> BoxFuture<'_, ()> {
        self.set(key, ClaimState::Held { reason });
        Box::pin(async {})
    }

    fn release(&self, key: NonceKey) -> BoxFuture<'_, ()> {
        let mut entries = self.entries();
        if matches!(
            entries.get(&key).map(|e| &e.state),
            Some(ClaimState::InFlight)
        ) {
            entries.remove(&key);
        }
        drop(entries);
        Box::pin(async {})
    }

    fn held(&self) -> BoxFuture<'_, Vec<(NonceKey, &'static str)>> {
        let held = self
            .entries()
            .iter()
            .filter_map(|(k, e)| match e.state {
                ClaimState::Held { reason } => Some((*k, reason)),
                _ => None,
            })
            .collect();
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
    async fn full_store_drops_only_expired_settled_claims() {
        let clock = ManualClock::at(100);
        let s = MemoryNonceStore::new(2, Arc::new(clock.clone()));
        assert_eq!(s.claim(key(1), 150).await, Claim::Fresh);
        s.settled(key(1), "0xabc".into()).await;
        assert_eq!(s.claim(key(2), 150).await, Claim::Fresh);
        s.hold(key(2), "settle_timeout").await;
        assert_eq!(
            s.claim(key(3), 150).await,
            Claim::Full,
            "nothing expired yet"
        );
        clock.set(151);
        assert_eq!(
            s.claim(key(3), 300).await,
            Claim::Fresh,
            "expired settled claim dropped"
        );
        assert_eq!(
            s.claim(key(4), 300).await,
            Claim::Full,
            "held and in-flight are never dropped"
        );
        assert_eq!(
            s.state(&key(2)),
            Some(ClaimState::Held {
                reason: "settle_timeout"
            })
        );
    }
}
