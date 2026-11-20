//! The HTTP header codec: base64 (standard alphabet) of UTF-8 JSON, in both directions.
//! Decoding is bounded before any allocation proportional to the input.

use base64::Engine as _;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The longest payment header accepted, encoded. A v2 exact-EVM
/// payload is about 1.5 KiB encoded; 8 KiB is the common per-header limit of proxies.
pub const MAX_HEADER_BYTES: usize = 8 * 1024; // LIMIT(payment-header)

const DECODER: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Why a payment header was refused. Carries no input bytes: the header is attacker text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    #[error("payment header longer than {MAX_HEADER_BYTES} bytes")]
    TooLong,
    #[error("payment header is not base64")]
    Base64,
    #[error("payment header is not the expected JSON: {0}")]
    Json(String),
}

pub fn encode_header<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let json = serde_json::to_vec(value)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(json))
}

pub fn decode_header<T: DeserializeOwned>(raw: &[u8]) -> Result<T, HeaderError> {
    if raw.len() > MAX_HEADER_BYTES {
        return Err(HeaderError::TooLong);
    }
    let trimmed = raw.trim_ascii();
    let json = DECODER.decode(trimmed).map_err(|_| HeaderError::Base64)?;
    serde_json::from_slice(&json).map_err(|e| HeaderError::Json(classify(&e)))
}

/// The serde error without the input echoed back: category and position only.
fn classify(e: &serde_json::Error) -> String {
    let kind = match e.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "shape",
        serde_json::error::Category::Eof => "truncated",
    };
    format!("{kind} at line {} column {}", e.line(), e.column())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PaymentRequired;

    #[test]
    fn refuses_oversized_before_decoding() {
        let raw = vec![b'A'; MAX_HEADER_BYTES + 1];
        assert_eq!(
            decode_header::<PaymentRequired>(&raw),
            Err(HeaderError::TooLong)
        );
    }

    #[test]
    fn refuses_non_base64_and_non_json() {
        assert_eq!(
            decode_header::<PaymentRequired>(b"%%%"),
            Err(HeaderError::Base64)
        );
        let not_json = base64::engine::general_purpose::STANDARD.encode(b"{\"x402Version\":");
        assert!(matches!(
            decode_header::<PaymentRequired>(not_json.as_bytes()),
            Err(HeaderError::Json(_))
        ));
    }

    #[test]
    fn error_text_never_echoes_input() {
        let secret =
            base64::engine::general_purpose::STANDARD.encode(b"{\"x402Version\":\"SECRET-VALUE\"}");
        let Err(e) = decode_header::<PaymentRequired>(secret.as_bytes()) else {
            panic!("must fail")
        };
        assert!(!e.to_string().contains("SECRET"), "{e}");
    }
}
