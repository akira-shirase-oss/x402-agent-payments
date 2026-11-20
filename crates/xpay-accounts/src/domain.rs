//! The kit's rules, with no I/O: emails, passwords, roles, plan limits.

use serde::{Deserialize, Serialize};

/// Shortest password accepted, in characters.
pub const MIN_PASSWORD_CHARS: usize = 12;
/// Longest password accepted, in bytes (Argon2 hashes any length; this bounds the work).
pub const MAX_PASSWORD_BYTES: usize = 512;

/// Trims and lower-cases `raw`, and checks it looks like one address. Not RFC 5322: just
/// enough that two spellings of one mailbox are one account and nothing odd is stored.
pub fn normalize_email(raw: &str) -> Option<String> {
    let email = raw.trim().to_lowercase();
    let (local, domain) = email.split_once('@')?;
    let plain = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_graphic() && c != '@');
    (email.len() <= 254
        && plain(local)
        && plain(domain)
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.'))
    .then_some(email)
}

/// Why a password is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordProblem {
    /// Fewer than [`MIN_PASSWORD_CHARS`] characters.
    TooShort,
    /// More than [`MAX_PASSWORD_BYTES`] bytes.
    TooLong,
}

/// Checks a new password's length (the only rule that does not just annoy people).
pub fn check_password(password: &str) -> Result<(), PasswordProblem> {
    if password.len() > MAX_PASSWORD_BYTES {
        Err(PasswordProblem::TooLong)
    } else if password.chars().count() < MIN_PASSWORD_CHARS {
        Err(PasswordProblem::TooShort)
    } else {
        Ok(())
    }
}

/// A member's role in a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Everything, including deleting the workspace and making owners.
    Owner,
    /// Members and tokens.
    Admin,
    /// Reads.
    Member,
}

impl Role {
    /// The stored form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    /// The stored form, parsed.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "member" => Some(Self::Member),
            _ => None,
        }
    }
}

/// What a request wants to do in a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// See the workspace.
    Read,
    /// List its members.
    ListMembers,
    /// Add or remove an admin or a member.
    ManageMembers,
    /// Add or remove an owner.
    ManageOwners,
    /// List, create and delete API tokens.
    ManageTokens,
    /// Delete the workspace.
    Delete,
}

/// The policy table: which role may do what.
pub const fn allows(role: Role, action: Action) -> bool {
    match action {
        Action::Read | Action::ListMembers => true,
        Action::ManageMembers | Action::ManageTokens => matches!(role, Role::Owner | Role::Admin),
        Action::ManageOwners | Action::Delete => matches!(role, Role::Owner),
    }
}

/// A plan's limits, checked at admission in the same transaction as the insert they bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanLimits {
    /// Members per workspace.
    pub max_members: u32,
    /// API tokens per workspace.
    pub max_tokens: u32,
    /// Workspaces one user may own.
    pub max_owned_workspaces: u32,
}

impl Default for PlanLimits {
    /// The free plan (payments are bypassed: everyone is on it).
    fn default() -> Self {
        Self {
            max_members: 10,
            max_tokens: 10,
            max_owned_workspaces: 3,
        }
    }
}

/// True when one more may be added to `current` under `limit`.
pub fn admits(current: i64, limit: u32) -> bool {
    current < i64::from(limit) // LIMIT(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn emails_are_normalized_and_odd_ones_refused() {
        assert_eq!(
            normalize_email("  Ada@Example.TEST "),
            Some("ada@example.test".to_owned())
        );
        for bad in [
            "",
            "ada",
            "@example.test",
            "ada@",
            "ada@example",
            "a b@example.test",
            "a@b@c.test",
            "ada@.test",
            "ada@test.",
        ] {
            assert_eq!(normalize_email(bad), None, "{bad:?}");
        }
        assert_eq!(
            normalize_email(&format!("{}@example.test", "a".repeat(250))),
            None
        );
    }

    #[test]
    fn passwords_are_bounded_by_length_only() {
        assert_eq!(check_password("short"), Err(PasswordProblem::TooShort));
        assert_eq!(check_password("correct horse battery"), Ok(()));
        assert_eq!(
            check_password(&"é".repeat(11)),
            Err(PasswordProblem::TooShort)
        );
        assert_eq!(
            check_password(&"a".repeat(513)),
            Err(PasswordProblem::TooLong)
        );
    }

    #[test]
    fn the_policy_table() {
        use Action::*;
        for (role, may) in [
            (Role::Owner, [true, true, true, true, true, true]),
            (Role::Admin, [true, true, true, false, true, false]),
            (Role::Member, [true, true, false, false, false, false]),
        ] {
            for (action, expected) in [
                Read,
                ListMembers,
                ManageMembers,
                ManageOwners,
                ManageTokens,
                Delete,
            ]
            .into_iter()
            .zip(may)
            {
                assert_eq!(allows(role, action), expected, "{role:?} {action:?}");
            }
        }
    }

    proptest! {
        /// At its edges: everything below the limit is admitted, the limit itself and
        /// anything past it is not, a zero limit admits nothing.
        #[test]
        fn plan_limits_admit_exactly_below_the_limit(limit in 0_u32..1_000, current in -2_i64..1_100) {
            prop_assert_eq!(admits(current, limit), current < i64::from(limit));
            if limit > 0 {
                prop_assert!(admits(i64::from(limit) - 1, limit));
            }
            prop_assert!(!admits(i64::from(limit), limit));
            prop_assert!(!admits(i64::from(limit) + 1, limit));
            prop_assert!(!admits(current.max(0), 0));
        }
    }
}
