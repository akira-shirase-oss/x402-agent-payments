//! CAIP-2 chain ids (`namespace:reference`), validated as the reference does:
//! `^[-a-z0-9]{3,8}:[-_a-zA-Z0-9]{1,32}$`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A CAIP-2 network id, e.g. `eip155:84532`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Caip2(String);

/// Why a string is not a CAIP-2 id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NetworkError {
    #[error("not a CAIP-2 id")]
    Malformed,
    #[error("not an eip155 network")]
    NotEvm,
}

impl Caip2 {
    /// Base Sepolia.
    pub const BASE_SEPOLIA: &'static str = "eip155:84532";
    /// Base mainnet.
    pub const BASE: &'static str = "eip155:8453";

    pub fn parse(s: &str) -> Result<Self, NetworkError> {
        let (ns, reference) = s.split_once(':').ok_or(NetworkError::Malformed)?;
        let ns_ok = (3..=8).contains(&ns.len())
            && ns
                .bytes()
                .all(|b| b == b'-' || b.is_ascii_lowercase() || b.is_ascii_digit());
        let ref_ok = (1..=32).contains(&reference.len())
            && reference
                .bytes()
                .all(|b| b == b'-' || b == b'_' || b.is_ascii_alphanumeric());
        if ns_ok && ref_ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(NetworkError::Malformed)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The EVM chain id of an `eip155:<decimal>` network.
    pub fn evm_chain_id(&self) -> Result<u64, NetworkError> {
        let (ns, reference) = self.0.split_once(':').ok_or(NetworkError::Malformed)?;
        if ns != "eip155" || reference.starts_with('0') {
            return Err(NetworkError::NotEvm);
        }
        reference.parse::<u64>().map_err(|_| NetworkError::NotEvm)
    }
}

impl fmt::Display for Caip2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Caip2 {
    type Err = NetworkError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Caip2 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Caip2 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_reference_shapes_and_refuses_the_rest() {
        for ok in [
            "eip155:84532",
            "eip155:8453",
            "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
        ] {
            assert!(Caip2::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "eip155",
            "base-sepolia",
            "EIP155:1",
            "ei:1",
            "eip155:",
            "eip155:1:2",
            "eip155:84532 ",
            "toolongns:1",
            "eip155:123456789012345678901234567890123",
        ] {
            assert_eq!(Caip2::parse(bad), Err(NetworkError::Malformed), "{bad}");
        }
    }

    #[test]
    fn evm_chain_id_only_for_canonical_eip155() {
        assert_eq!(
            Caip2::parse("eip155:84532").and_then(|c| c.evm_chain_id()),
            Ok(84532)
        );
        for bad in [
            "eip155:084532",
            "eip155:abc",
            "solana:1",
            "eip155:99999999999999999999",
        ] {
            assert!(
                Caip2::parse(bad).and_then(|c| c.evm_chain_id()).is_err(),
                "{bad}"
            );
        }
    }
}
