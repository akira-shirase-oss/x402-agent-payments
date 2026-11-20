//! Sign-in admission: a budget per client address, a global budget, and a path for the
//! right credential that floods cannot block (Service Monitor's `throttle.rs`, Intent
//! Studio's `SignInGate` and D-052, Colophon D-154/D-162, distilled).
//!
//! 1. **Per address, first and alone.** Every attempt takes a token from its address's
//!    bucket before the password is compared; an empty bucket is 429 and nothing is
//!    verified, so a throttled address learns nothing. One address's flood touches only
//!    its own bucket.
//! 2. **Global, for guesses spread over many addresses.** An attempt from a (user,
//!    address) pair that has not signed in successfully takes a global token right before
//!    the password is verified. A right password gives both tokens back, so the budgets
//!    count failed verifications only.
//! 3. **Trusted pairs skip the global budget.** A pair that signed in within the trust
//!    window (`sign_in_trust`) never touches the global bucket: a distributed flood that
//!    drains it cannot lock the owner out of a device they already use. A new device
//!    waits for the flood to pass.
//!
//! Password work is bounded separately (`password.rs`), and the global budget bounds how
//! much of it untrusted attempts can cause.

use std::time::Duration;

use xpay_http::budget::{Global, PerKey, Rate, RetryAfter};
use xpay_http::client::ClientKey;

/// The budgets' shapes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignInRates {
    /// Each address: 5 back to back, then one every 12 s (5 a minute).
    pub per_address: Rate,
    /// Everyone together, untrusted pairs only: 20, then one every 3 s (20 a minute).
    pub global: Rate,
    /// How long a successful sign-in makes its (user, address) pair trusted.
    pub trust_window: Duration,
}

impl Default for SignInRates {
    fn default() -> Self {
        Self {
            per_address: Rate::new(5, Duration::from_secs(12)),
            global: Rate::new(20, Duration::from_secs(3)),
            trust_window: Duration::from_hours(30 * 24),
        }
    }
}

/// The emails budgets (sign-up, resend, reset), which bound mail a client can cause.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MailRates {
    /// Each address: 3, then one every 2 minutes.
    pub per_address: Rate,
    /// Everyone together: 30, then one every 10 s.
    pub global: Rate,
}

impl Default for MailRates {
    fn default() -> Self {
        Self {
            per_address: Rate::new(3, Duration::from_secs(120)),
            global: Rate::new(30, Duration::from_secs(10)),
        }
    }
}

/// Addresses tracked per budget (about 40 bytes each).
pub const TRACKED_ADDRESSES: usize = 16_384;

/// The sign-in gate. Cheap to clone.
#[derive(Debug, Clone)]
pub struct SignInGate {
    per_address: PerKey<ClientKey>,
    global: Global,
}

/// What an admitted attempt holds: give it back on a right password.
#[derive(Debug)]
#[must_use = "an attempt is given back on success, kept spent on failure"]
pub struct Attempt {
    address: ClientKey,
    global: bool,
}

impl SignInGate {
    /// A gate with these rates.
    pub fn new(rates: SignInRates) -> Self {
        Self {
            per_address: PerKey::new(rates.per_address, TRACKED_ADDRESSES),
            global: Global::new(rates.global),
        }
    }

    /// Step 1: the address's budget.
    pub fn admit(&self, address: ClientKey) -> Result<Attempt, RetryAfter> {
        self.per_address.take(address)?; // LIMIT(sign-in-per-address)
        Ok(Attempt {
            address,
            global: false,
        })
    }

    /// Step 2, for an untrusted pair: the global budget, right before verifying. Refused,
    /// the address's token comes back (nothing was verified).
    pub fn reserve_global(&self, attempt: &mut Attempt) -> Result<(), RetryAfter> {
        let global = self.global.take(); // LIMIT(sign-in-global)
        if let Err(wait) = global {
            self.per_address.give_back(attempt.address);
            return Err(wait);
        }
        attempt.global = true;
        Ok(())
    }

    /// A right password: every token the attempt took comes back. Takes the attempt, so
    /// one attempt cannot be given back twice.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "consuming the attempt is the point: it cannot be refunded twice"
    )]
    pub fn succeeded(&self, attempt: Attempt) {
        let Attempt { address, global } = attempt;
        self.per_address.give_back(address);
        if global {
            self.global.give_back();
        }
    }
}

/// The mail budgets. Cheap to clone.
#[derive(Debug, Clone)]
pub struct MailGate {
    per_address: PerKey<ClientKey>,
    global: Global,
}

impl MailGate {
    /// A gate with these rates.
    pub fn new(rates: MailRates) -> Self {
        Self {
            per_address: PerKey::new(rates.per_address, TRACKED_ADDRESSES),
            global: Global::new(rates.global),
        }
    }

    /// Takes one from the address, then one from everyone (the address's comes back if
    /// the global one is spent).
    pub fn admit(&self, address: ClientKey) -> Result<(), RetryAfter> {
        self.per_address.take(address)?; // LIMIT(mail-per-address)
        if let Err(wait) = self.global.take() {
            self.per_address.give_back(address);
            return Err(wait);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn key(n: u32) -> ClientKey {
        ClientKey::V4(Ipv4Addr::from(0xC633_6400 + n))
    }

    #[tokio::test(start_paused = true)]
    async fn refusals_per_address_never_spend_the_global_budget() {
        let gate = SignInGate::new(SignInRates::default());
        let mut admitted = 0;
        for _ in 0..25 {
            if let Ok(mut attempt) = gate.admit(key(1)) {
                admitted += 1;
                gate.reserve_global(&mut attempt).unwrap();
            }
        }
        assert_eq!(admitted, 5);
        // 15 of the 20 global tokens are left: the 20 refusals took none.
        let mut left = 0;
        for n in 2..40 {
            if let Ok(mut attempt) = gate.admit(key(n))
                && gate.reserve_global(&mut attempt).is_ok()
            {
                left += 1;
            }
        }
        assert_eq!(left, 15);
    }

    #[tokio::test(start_paused = true)]
    async fn a_right_password_gives_every_token_back() {
        let gate = SignInGate::new(SignInRates::default());
        for _ in 0..100 {
            let mut attempt = gate.admit(key(1)).expect("never spent");
            gate.reserve_global(&mut attempt).expect("never spent");
            gate.succeeded(attempt);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_spent_global_budget_returns_the_address_token() {
        let gate = SignInGate::new(SignInRates::default());
        for n in 0..20 {
            let mut attempt = gate.admit(key(n)).unwrap();
            gate.reserve_global(&mut attempt).unwrap();
        }
        // Key 100 is refused globally five times, and still has its whole burst.
        for _ in 0..5 {
            let mut attempt = gate.admit(key(100)).unwrap();
            assert!(gate.reserve_global(&mut attempt).is_err());
        }
        let _ = gate
            .admit(key(100))
            .expect("the address's tokens came back");
    }

    #[tokio::test(start_paused = true)]
    async fn mail_is_bounded_per_address_and_globally() {
        let gate = MailGate::new(MailRates::default());
        assert_eq!((0..10).filter(|_| gate.admit(key(1)).is_ok()).count(), 3);
        let spread = (2..100).filter(|n| gate.admit(key(*n)).is_ok()).count();
        assert_eq!(spread, 27, "30 in all");
    }
}
