//! x402 v2: the wire types, the HTTP header codec, CAIP-2 network ids, the exact EVM scheme
//! (EIP-3009 `transferWithAuthorization` on USDC) and the facilitator contract.
//!
//! The wire types keep amounts and addresses as the strings the protocol carries, so what
//! is parsed re-emits unchanged; [`exact`] parses them into typed values to check them.

pub mod clock;
pub mod codec;
pub mod exact;
pub mod facilitator;
pub mod network;
pub mod reason;
pub mod types;

pub use codec::{HeaderError, MAX_HEADER_BYTES, decode_header, encode_header};
pub use network::{Caip2, NetworkError};
pub use types::{
    FacilitatorRequest, PaymentPayload, PaymentRequired, PaymentRequirements, ResourceInfo,
    SettleResponse, SupportedKind, SupportedResponse, VerifyResponse,
};

/// The protocol version this crate speaks.
pub const X402_VERSION: u32 = 2;

/// Server → client: the base64 `PaymentRequired`, on a 402.
pub const PAYMENT_REQUIRED_HEADER: &str = "payment-required";
/// Client → server: the base64 `PaymentPayload`.
pub const PAYMENT_SIGNATURE_HEADER: &str = "payment-signature";
/// Server → client: the base64 `SettleResponse`.
pub const PAYMENT_RESPONSE_HEADER: &str = "payment-response";
/// The only scheme this crate implements.
pub const SCHEME_EXACT: &str = "exact";
