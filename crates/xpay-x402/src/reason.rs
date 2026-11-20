//! The reference's error/invalid-reason codes this crate emits (x402 v2 spec §9).

pub const INSUFFICIENT_FUNDS: &str = "insufficient_funds";
pub const VALID_AFTER: &str = "invalid_exact_evm_payload_authorization_valid_after";
pub const VALID_BEFORE: &str = "invalid_exact_evm_payload_authorization_valid_before";
pub const VALUE_MISMATCH: &str = "invalid_exact_evm_payload_authorization_value_mismatch";
pub const SIGNATURE: &str = "invalid_exact_evm_payload_signature";
pub const RECIPIENT_MISMATCH: &str = "invalid_exact_evm_payload_recipient_mismatch";
pub const NETWORK: &str = "invalid_network";
/// The payload's `accepted.network` is not the requirements' network.
pub const NETWORK_MISMATCH: &str = "network_mismatch";
pub const PAYLOAD: &str = "invalid_payload";
pub const REQUIREMENTS: &str = "invalid_payment_requirements";
pub const SCHEME: &str = "unsupported_scheme";
pub const VERSION: &str = "invalid_x402_version";
pub const TRANSACTION_STATE: &str = "invalid_transaction_state";
pub const VERIFY_ERROR: &str = "unexpected_verify_error";
pub const SETTLE_ERROR: &str = "unexpected_settle_error";
pub const SETTLEMENT_PENDING: &str = "settlement_pending";
/// The (payer, nonce) was already used (reference: the EVM facilitator's nonce check).
pub const NONCE_REUSED: &str = "invalid_exact_evm_nonce_already_used";
