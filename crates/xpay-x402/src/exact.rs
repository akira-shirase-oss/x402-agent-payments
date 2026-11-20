//! The exact scheme on EVM: an EIP-3009 `transferWithAuthorization` on USDC, signed as
//! EIP-712 typed data under the token's own domain. The domain name differs per network
//! (`docs/KNOWLEDGE.md`): "USDC" on Base Sepolia, "USD Coin" on Base; the version is "2".

use std::borrow::Cow;
use std::str::FromStr as _;

use alloy_primitives::{Address, B256, Signature, U256};
use alloy_signer::SignerSync;
use alloy_sol_types::{Eip712Domain, SolStruct as _};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::SCHEME_EXACT;
use crate::network::Caip2;
use crate::reason;
use crate::types::{PaymentPayload, PaymentRequirements};

alloy_sol_types::sol! {
    /// EIP-3009's typed struct, field order and types as the token contract hashes them.
    #[derive(Debug)]
    struct TransferWithAuthorization {
        address from;
        address to;
        uint256 value;
        uint256 validAfter;
        uint256 validBefore;
        bytes32 nonce;
    }
}

/// The reference facilitator refuses an authorisation that expires within this many
/// seconds: it must survive the time to land on chain.
pub const VALID_BEFORE_MARGIN_SECS: u64 = 6;

/// One USDC deployment: where it lives and the EIP-712 domain it signs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsdcDeployment {
    pub network: &'static str,
    pub address: &'static str,
    pub domain_name: &'static str,
    pub domain_version: &'static str,
}

/// The networks this crate knows USDC on. Checked against `docs/KNOWLEDGE.md`.
pub const USDC: [UsdcDeployment; 2] = [
    UsdcDeployment {
        network: Caip2::BASE_SEPOLIA,
        address: "0x036CbD53842c5426634e7929541eC2318f3dCF7e",
        domain_name: "USDC",
        domain_version: "2",
    },
    UsdcDeployment {
        network: Caip2::BASE,
        address: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
        domain_name: "USD Coin",
        domain_version: "2",
    },
];

pub fn usdc(network: &Caip2) -> Option<&'static UsdcDeployment> {
    USDC.iter().find(|d| d.network == network.as_str())
}

/// Requirements for `amount` atomic USDC on `network`, to `pay_to`, with the domain in
/// `extra` so the buyer signs under the right name.
pub fn usdc_requirements(
    network: &Caip2,
    amount: U256,
    pay_to: Address,
    max_timeout_seconds: u64,
) -> Option<PaymentRequirements> {
    let d = usdc(network)?;
    let mut extra = Map::new();
    extra.insert("name".into(), Value::from(d.domain_name));
    extra.insert("version".into(), Value::from(d.domain_version));
    Some(PaymentRequirements {
        scheme: SCHEME_EXACT.into(),
        network: network.clone(),
        amount: amount.to_string(),
        asset: d.address.into(),
        pay_to: pay_to.to_checksum(None),
        max_timeout_seconds,
        extra: Some(extra),
    })
}

/// The `authorization` object of the payload, as on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Authorization {
    pub from: String,
    pub to: String,
    pub value: String,
    pub valid_after: String,
    pub valid_before: String,
    pub nonce: String,
}

/// The exact-EVM `payload` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExactEvmPayload {
    pub signature: String,
    pub authorization: Authorization,
}

impl ExactEvmPayload {
    pub fn from_payload(p: &PaymentPayload) -> Result<Self, &'static str> {
        serde_json::from_value(Value::Object(p.payload.clone())).map_err(|_| reason::PAYLOAD)
    }

    pub fn to_map(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        }
    }
}

/// An authorisation with every field parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypedAuthorization {
    pub from: Address,
    pub to: Address,
    pub value: U256,
    pub valid_after: U256,
    pub valid_before: U256,
    pub nonce: B256,
}

impl TypedAuthorization {
    pub fn parse(a: &Authorization) -> Result<Self, &'static str> {
        let addr = |s: &str| Address::from_str(s).map_err(|_| reason::PAYLOAD);
        let uint = |s: &str| {
            if s.is_empty() || s.len() > 78 || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(reason::PAYLOAD);
            }
            U256::from_str_radix(s, 10).map_err(|_| reason::PAYLOAD)
        };
        let nonce = a.nonce.strip_prefix("0x").ok_or(reason::PAYLOAD)?;
        if nonce.len() != 64 {
            return Err(reason::PAYLOAD);
        }
        Ok(Self {
            from: addr(&a.from)?,
            to: addr(&a.to)?,
            value: uint(&a.value)?,
            valid_after: uint(&a.valid_after)?,
            valid_before: uint(&a.valid_before)?,
            nonce: B256::from_str(nonce).map_err(|_| reason::PAYLOAD)?,
        })
    }

    pub fn to_wire(&self) -> Authorization {
        Authorization {
            from: self.from.to_checksum(None),
            to: self.to.to_checksum(None),
            value: self.value.to_string(),
            valid_after: self.valid_after.to_string(),
            valid_before: self.valid_before.to_string(),
            nonce: self.nonce.to_string(),
        }
    }

    fn as_sol(&self) -> TransferWithAuthorization {
        TransferWithAuthorization {
            from: self.from,
            to: self.to,
            value: self.value,
            validAfter: self.valid_after,
            validBefore: self.valid_before,
            nonce: self.nonce,
        }
    }
}

/// The token's EIP-712 domain, from the requirements (`extra.name`, `extra.version`, the
/// asset as verifying contract, the network's chain id).
pub fn domain(req: &PaymentRequirements) -> Result<Eip712Domain, &'static str> {
    let chain_id = req.network.evm_chain_id().map_err(|_| reason::NETWORK)?;
    let extra = req.extra.as_ref().ok_or(reason::REQUIREMENTS)?;
    let text = |k: &str| {
        extra
            .get(k)
            .and_then(Value::as_str)
            .map(|s| Cow::Owned(s.to_owned()))
            .ok_or(reason::REQUIREMENTS)
    };
    let asset = Address::from_str(&req.asset).map_err(|_| reason::REQUIREMENTS)?;
    Ok(Eip712Domain::new(
        Some(text("name")?),
        Some(text("version")?),
        Some(U256::from(chain_id)),
        Some(asset),
        None,
    ))
}

pub fn signing_hash(auth: &TypedAuthorization, domain: &Eip712Domain) -> B256 {
    auth.as_sol().eip712_signing_hash(domain)
}

/// Signs `auth` for `req` and returns the payload object.
pub fn sign(
    signer: &(dyn SignerSync + Send + Sync),
    auth: &TypedAuthorization,
    req: &PaymentRequirements,
) -> Result<ExactEvmPayload, SignError> {
    let domain = domain(req).map_err(SignError::Requirements)?;
    let sig = signer
        .sign_hash_sync(&signing_hash(auth, &domain))
        .map_err(|e| SignError::Signer(e.to_string()))?;
    Ok(ExactEvmPayload {
        signature: format!("0x{}", alloy_primitives::hex::encode(sig.as_bytes())),
        authorization: auth.to_wire(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignError {
    #[error("requirements cannot be signed: {0}")]
    Requirements(&'static str),
    #[error("signer failed: {0}")]
    Signer(String),
}

/// The EOA that signed `hash`. Refuses anything but a 65-byte low-s signature.
pub fn recover(signature: &str, hash: &B256) -> Result<Address, &'static str> {
    let hex = signature.strip_prefix("0x").ok_or(reason::SIGNATURE)?;
    if hex.len() != 130 {
        return Err(reason::SIGNATURE);
    }
    let bytes = alloy_primitives::hex::decode(hex).map_err(|_| reason::SIGNATURE)?;
    // USDC's ecrecover takes v = 27 or 28 (0 and 1 are the same parity in another
    // spelling); anything else is a different string for the same signature.
    if !matches!(bytes.get(64), Some(0 | 1 | 27 | 28)) {
        return Err(reason::SIGNATURE);
    }
    let sig = Signature::from_raw(&bytes).map_err(|_| reason::SIGNATURE)?;
    if sig.normalize_s().is_some() {
        return Err(reason::SIGNATURE);
    }
    sig.recover_address_from_prehash(hash)
        .map_err(|_| reason::SIGNATURE)
}

/// What a payment proves once every check passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    pub payer: Address,
    pub nonce: B256,
    pub value: U256,
    pub valid_before: U256,
}

/// Every offline check of an exact-EVM payment against the seller's `req` at unix time
/// `now`, in the reference facilitator's order. Chain state (balance, nonce used on chain)
/// is the facilitator's to check.
pub fn check(
    payment: &PaymentPayload,
    req: &PaymentRequirements,
    now: u64,
) -> Result<Verified, &'static str> {
    if payment.x402_version != crate::X402_VERSION {
        return Err(reason::VERSION);
    }
    if payment.accepted.scheme != SCHEME_EXACT || req.scheme != SCHEME_EXACT {
        return Err(reason::SCHEME);
    }
    if payment.accepted.network != req.network {
        return Err(reason::NETWORK_MISMATCH);
    }
    let domain = domain(req)?;
    if !payment.accepted.asset.eq_ignore_ascii_case(&req.asset) {
        return Err(reason::REQUIREMENTS);
    }
    let payload = ExactEvmPayload::from_payload(payment)?;
    let auth = TypedAuthorization::parse(&payload.authorization)?;
    let pay_to = Address::from_str(&req.pay_to).map_err(|_| reason::REQUIREMENTS)?;
    if auth.to != pay_to {
        return Err(reason::RECIPIENT_MISMATCH);
    }
    let amount = U256::from_str_radix(&req.amount, 10).map_err(|_| reason::REQUIREMENTS)?;
    if auth.value != amount {
        return Err(reason::VALUE_MISMATCH);
    }
    let now = U256::from(now);
    if auth.valid_before < now + U256::from(VALID_BEFORE_MARGIN_SECS) {
        return Err(reason::VALID_BEFORE);
    }
    if auth.valid_after > now {
        return Err(reason::VALID_AFTER);
    }
    if recover(&payload.signature, &signing_hash(&auth, &domain))? != auth.from {
        return Err(reason::SIGNATURE);
    }
    Ok(Verified {
        payer: auth.from,
        nonce: auth.nonce,
        value: auth.value,
        valid_before: auth.valid_before,
    })
}

/// How far back the buyer dates `validAfter`: USDC refuses `validAfter >= block.timestamp`,
/// and a buyer clock slightly ahead of the chain's would make `now` fail
/// (`docs/KNOWLEDGE.md`).
pub const VALID_AFTER_BACKDATE_SECS: u64 = 600;

/// The buyer's half: an authorisation of exactly `req.amount` to `req.pay_to`, valid from
/// `now - VALID_AFTER_BACKDATE_SECS` to `now + req.max_timeout_seconds`, signed and wrapped
/// as the `PAYMENT-SIGNATURE` payload.
pub fn pay(
    signer: &(dyn SignerSync + Send + Sync),
    from: Address,
    req: &PaymentRequirements,
    resource: Option<crate::ResourceInfo>,
    now: u64,
    nonce: B256,
) -> Result<PaymentPayload, SignError> {
    let parse =
        |s: &str| Address::from_str(s).map_err(|_| SignError::Requirements(reason::REQUIREMENTS));
    let auth = TypedAuthorization {
        from,
        to: parse(&req.pay_to)?,
        value: U256::from_str_radix(&req.amount, 10)
            .map_err(|_| SignError::Requirements(reason::REQUIREMENTS))?,
        valid_after: U256::from(now.saturating_sub(VALID_AFTER_BACKDATE_SECS)),
        valid_before: U256::from(now.saturating_add(req.max_timeout_seconds)),
        nonce,
    };
    let payload = sign(signer, &auth, req)?;
    Ok(PaymentPayload {
        x402_version: crate::X402_VERSION,
        resource,
        accepted: req.clone(),
        payload: payload.to_map(),
        extensions: None,
    })
}

#[cfg(test)]
mod v_byte_tests {
    use super::*;

    #[test]
    fn only_v_27_28_0_1_are_accepted() {
        let signer =
            alloy_signer_local::PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).unwrap();
        let hash = B256::repeat_byte(0x42);
        let sig = signer.sign_hash_sync(&hash).unwrap().as_bytes();
        for v in [0u8, 1, 27, 28, 59, 60, 2, 255] {
            let mut b = sig;
            b[64] = v;
            let parity_ok = (b[64] == 27 || b[64] == 0) == (sig[64] == 27);
            let got = recover(&format!("0x{}", alloy_primitives::hex::encode(b)), &hash);
            match v {
                0 | 1 | 27 | 28 if parity_ok => assert_eq!(got, Ok(signer.address()), "v={v}"),
                0 | 1 | 27 | 28 => assert_ne!(got, Ok(signer.address()), "v={v}"),
                _ => assert_eq!(got, Err(reason::SIGNATURE), "v={v}"),
            }
        }
    }
}
