//! Unix time in seconds, injectable so authorisation windows are tested without sleeping.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> u64;
}

/// The system clock. A clock before 1970 reads as 0, which fails every window check closed.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}

/// A clock tests set by hand.
#[derive(Debug, Clone, Default)]
pub struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    pub fn at(now: u64) -> Self {
        Self(Arc::new(AtomicU64::new(now)))
    }
    pub fn set(&self, now: u64) {
        self.0.store(now, Ordering::SeqCst);
    }
    pub fn advance(&self, secs: u64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
