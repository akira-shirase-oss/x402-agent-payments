//! Bounded password work (Colophon D-150, Intent Studio): every Argon2id hash and verify
//! runs off the async workers, at most `concurrency` at once; a request that cannot get a
//! slot within [`WAIT`] is answered 503 `overloaded` before it costs anything else. A
//! missing account is verified against a dummy hash, so it takes the same work as a wrong
//! password.

use std::sync::Arc;
use std::time::Duration;

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher as _, PasswordVerifier as _};
use tokio::sync::Semaphore;

use crate::error::AccountsError;
use crate::tokens::random;

/// Longest a request waits for a hashing slot.
pub const WAIT: Duration = Duration::from_secs(2);

/// The gate. Cheap to clone.
#[derive(Debug, Clone)]
pub struct PasswordGate {
    slots: Arc<Semaphore>,
    dummy: Arc<str>,
}

fn hash_now(password: &str) -> Result<String, AccountsError> {
    let salt =
        SaltString::encode_b64(&random::<16>().map_err(|e| AccountsError::Internal(e.into()))?)
            .map_err(|e| AccountsError::Internal(anyhow::anyhow!("salt: {e}")))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| AccountsError::Internal(anyhow::anyhow!("hashing: {e}")))
}

impl PasswordGate {
    /// A gate of `concurrency` slots, with a dummy hash made now.
    pub async fn new(concurrency: usize) -> Result<Self, AccountsError> {
        let dummy = tokio::task::spawn_blocking(|| {
            let throwaway =
                hex::encode(random::<16>().map_err(|e| AccountsError::Internal(e.into()))?);
            hash_now(&throwaway)
        })
        .await
        .map_err(|e| AccountsError::Internal(e.into()))??;
        Ok(Self {
            slots: Arc::new(Semaphore::new(concurrency.max(1))), // LIMIT(password-concurrency)
            dummy: Arc::from(dummy),
        })
    }

    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, AccountsError> {
        let permit = tokio::time::timeout(WAIT, self.slots.clone().acquire_owned())
            .await
            .map_err(|_| AccountsError::Overloaded)?
            .map_err(|_| AccountsError::Overloaded)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .map_err(|e| AccountsError::Internal(e.into()))
    }

    /// An Argon2id PHC string for `password`.
    pub async fn hash(&self, password: String) -> Result<String, AccountsError> {
        self.run(move || hash_now(&password)).await?
    }

    /// True when `password` matches `stored`; `None` verifies against the dummy hash and is
    /// always false.
    pub async fn verify(
        &self,
        password: String,
        stored: Option<String>,
    ) -> Result<bool, AccountsError> {
        let real = stored.is_some();
        let hash = stored.unwrap_or_else(|| self.dummy.to_string());
        let matched = self
            .run(move || {
                PasswordHash::new(&hash).is_ok_and(|parsed| {
                    Argon2::default()
                        .verify_password(password.as_bytes(), &parsed)
                        .is_ok()
                })
            })
            .await?;
        Ok(real && matched)
    }

    /// Slots free now (tests).
    pub fn available(&self) -> usize {
        self.slots.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn at_most_the_concurrency_hashes_at_once() {
        let gate = PasswordGate::new(1).await.unwrap();
        let mut running = Vec::new();
        for n in 0..3 {
            let gate = gate.clone();
            running.push(tokio::spawn(async move {
                gate.hash(format!("password number {n}")).await
            }));
        }
        let mut saw_full = false;
        for _ in 0..2_000 {
            if gate.available() == 0 {
                saw_full = true;
                break;
            }
            tokio::time::sleep(Duration::from_micros(200)).await;
        }
        for task in running {
            assert!(task.await.unwrap().unwrap().starts_with("$argon2id$"));
        }
        assert!(
            saw_full,
            "hashing never held the only slot: the gate is not bounding the work"
        );
        assert_eq!(gate.available(), 1);
        assert!(
            gate.verify("x".to_owned(), None).await.is_ok_and(|ok| !ok),
            "the dummy never matches"
        );
    }
}
