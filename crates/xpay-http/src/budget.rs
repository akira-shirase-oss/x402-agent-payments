//! Bounded token buckets: one per client key ([`PerKey`]) and one for everyone
//! ([`Global`]).
//!
//! A bucket holds at most `burst` tokens and earns one back every `refill_every`. Taking
//! from an empty bucket is refused with how long until a token is back; the caller answers
//! 429 with `Retry-After`. A taken token can be given back (a right password costs its
//! owner nothing); one not given back stays spent.
//!
//! Memory is bounded: a [`PerKey`] table tracks at most `capacity` keys. A full table first
//! forgets keys whose bucket has refilled (they are no different from new ones), then the
//! fullest bucket: the least-throttled client loses the least by being forgotten, and a
//! heavily throttled one survives churn (Service Monitor's `throttle.rs`).
//!
//! Time is `tokio::time::Instant`, so tests run on paused time.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

/// A bucket's shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    /// Tokens a full bucket holds (taken back to back).
    pub burst: u32,
    /// One token comes back this often.
    pub refill_every: Duration,
}

impl Rate {
    /// `burst` at once, then one every `refill_every`.
    pub const fn new(burst: u32, refill_every: Duration) -> Self {
        Self {
            burst,
            refill_every,
        }
    }

    fn burst_f64(self) -> f64 {
        f64::from(self.burst.max(1))
    }

    fn refilled(self, bucket: Bucket, now: Instant) -> f64 {
        let earned = now.saturating_duration_since(bucket.at).as_secs_f64()
            / self.refill_every.as_secs_f64().max(f64::MIN_POSITIVE);
        (bucket.tokens + earned).min(self.burst_f64())
    }

    fn full(self, now: Instant) -> Bucket {
        Bucket {
            tokens: self.burst_f64(),
            at: now,
        }
    }

    /// Takes one token, or says how long until one is back.
    fn take(self, bucket: &mut Bucket, now: Instant) -> Result<(), RetryAfter> {
        let tokens = self.refilled(*bucket, now);
        if tokens < 1.0 {
            *bucket = Bucket { tokens, at: now };
            let wait = (1.0 - tokens) * self.refill_every.as_secs_f64();
            return Err(RetryAfter(Duration::from_secs_f64(wait)));
        }
        *bucket = Bucket {
            tokens: tokens - 1.0,
            at: now,
        };
        Ok(())
    }

    fn give_back(self, bucket: &mut Bucket, now: Instant) {
        *bucket = Bucket {
            tokens: (self.refilled(*bucket, now) + 1.0).min(self.burst_f64()),
            at: now,
        };
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

/// How long until a token is back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryAfter(pub Duration);

impl RetryAfter {
    /// Whole seconds, rounded up, at least 1 (the `Retry-After` header's unit).
    pub fn secs(self) -> u64 {
        (self.0.as_secs() + u64::from(self.0.subsec_nanos() > 0)).max(1)
    }
}

/// One bucket per key, bounded. Cheap to clone (one shared table).
#[derive(Debug, Clone)]
pub struct PerKey<K> {
    table: Arc<Mutex<HashMap<K, Bucket>>>,
    rate: Rate,
    capacity: usize,
}

impl<K: Eq + Hash + Copy> PerKey<K> {
    /// A table of at most `capacity` keys.
    pub fn new(rate: Rate, capacity: usize) -> Self {
        Self {
            table: Arc::new(Mutex::new(HashMap::new())),
            rate,
            capacity: capacity.max(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<K, Bucket>> {
        // Plain numbers: a panic mid-update cannot leave the table inconsistent.
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes one token for `key`.
    pub fn take(&self, key: K) -> Result<(), RetryAfter> {
        let now = Instant::now();
        let rate = self.rate;
        let mut table = self.lock();
        if !table.contains_key(&key) && table.len() >= self.capacity {
            // LIMIT(per-key-table)
            make_room(&mut table, rate, now);
        }
        let bucket = table.entry(key).or_insert_with(|| rate.full(now));
        rate.take(bucket, now)
    }

    /// Gives one token back to `key` (a no-op for a key no longer tracked).
    pub fn give_back(&self, key: K) {
        let now = Instant::now();
        if let Some(bucket) = self.lock().get_mut(&key) {
            self.rate.give_back(bucket, now);
        }
    }

    /// Keys tracked now.
    pub fn tracked(&self) -> usize {
        self.lock().len()
    }
}

fn make_room<K: Eq + Hash + Copy>(table: &mut HashMap<K, Bucket>, rate: Rate, now: Instant) {
    let full = rate.burst_f64();
    table.retain(|_, bucket| rate.refilled(*bucket, now) < full);
    if table.is_empty() {
        return;
    }
    let fullest = table
        .iter()
        .max_by(|a, b| {
            rate.refilled(*a.1, now)
                .total_cmp(&rate.refilled(*b.1, now))
        })
        .map(|(key, _)| *key);
    if let Some(key) = fullest {
        table.remove(&key);
    }
}

/// One bucket for everyone. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Global {
    bucket: Arc<Mutex<Bucket>>,
    rate: Rate,
}

impl Global {
    /// A full bucket.
    pub fn new(rate: Rate) -> Self {
        Self {
            bucket: Arc::new(Mutex::new(rate.full(Instant::now()))),
            rate,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Bucket> {
        self.bucket.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes one token.
    pub fn take(&self) -> Result<(), RetryAfter> {
        let now = Instant::now();
        self.rate.take(&mut self.lock(), now)
    }

    /// Gives one token back.
    pub fn give_back(&self) {
        let now = Instant::now();
        self.rate.give_back(&mut self.lock(), now);
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    const RATE: Rate = Rate::new(5, Duration::from_secs(2));

    #[tokio::test(start_paused = true)]
    async fn a_given_back_token_costs_nothing_and_a_spent_one_stays_spent() {
        let table = PerKey::new(RATE, 64);
        for _ in 0..50 {
            table.take(1).expect("given back every time");
            table.give_back(1);
        }
        for _ in 0..5 {
            table.take(1).expect("burst");
        }
        let wait = table.take(1).expect_err("spent");
        assert!(wait.0 <= RATE.refill_every && wait.0 > Duration::ZERO);
        assert_eq!(wait.secs(), 2);
        tokio::time::advance(RATE.refill_every).await;
        table.take(1).expect("one back");
        assert!(table.take(1).is_err());
        // Another key is untouched by key 1's spending.
        table.take(2).expect("its own bucket");
    }

    #[tokio::test(start_paused = true)]
    async fn the_table_never_grows_past_its_capacity_and_keeps_the_throttled() {
        let table = PerKey::new(RATE, 64);
        let hot = Ipv4Addr::new(203, 0, 113, 1);
        for _ in 0..5 {
            table.take(hot).expect("burst");
        }
        for n in 0..10_000_u32 {
            let _ = table.take(Ipv4Addr::from(0xC633_6400 + n));
            assert!(table.tracked() <= 64);
        }
        assert!(
            table.take(hot).is_err(),
            "evicting it would forgive its guesses"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_global_bucket_refills_at_its_rate() {
        let global = Global::new(Rate::new(20, Duration::from_secs(3)));
        for _ in 0..20 {
            global.take().expect("burst");
        }
        assert!(global.take().is_err());
        global.give_back();
        global.take().expect("given back");
        tokio::time::advance(Duration::from_millis(2_999)).await;
        assert!(global.take().is_err());
        tokio::time::advance(Duration::from_millis(1)).await;
        global.take().expect("one back after 3 s");
    }

    #[test]
    fn retry_after_is_whole_seconds_at_least_one() {
        assert_eq!(RetryAfter(Duration::from_millis(1)).secs(), 1);
        assert_eq!(RetryAfter(Duration::from_millis(1500)).secs(), 2);
        assert_eq!(RetryAfter(Duration::from_secs(2)).secs(), 2);
        assert_eq!(RetryAfter(Duration::ZERO).secs(), 1);
    }
}
