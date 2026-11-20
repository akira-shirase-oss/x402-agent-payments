//! Secrets the kit hands out, and their digests. Every secret is random from the operating
//! system and is stored only as its SHA-256 digest; it is shown once and never logged.
//!
//! - session id: 32 bytes, hex (the cookie value);
//! - email link token: 32 bytes, hex (verification and reset links);
//! - invite code: `inv_` + 32 base32 characters (160 bits);
//! - API token: [`API_TOKEN_PREFIX`] + 12 base32 characters (the display prefix, 60 bits,
//!   which identifies the token in listings) + `_` + 32 base32 characters (160 bits).

use sha2::{Digest as _, Sha256};

/// The leading text of every API token. A copy picks its own (`dr_`, `sm_`), so a leaked
/// token says which product it belongs to.
pub const API_TOKEN_PREFIX: &str = "xp_";

/// Random bytes from the operating system.
pub fn random<const N: usize>() -> Result<[u8; N], getrandom::Error> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes)?;
    Ok(bytes)
}

/// The SHA-256 digest of a secret's text.
pub fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// RFC 4648 base32, lower case, no padding.
pub fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((buffer >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(ALPHABET[((buffer << (5 - bits)) & 31) as usize]));
    }
    out
}

/// A new session id or email link token.
pub fn new_hex_secret() -> Result<String, getrandom::Error> {
    Ok(hex::encode(random::<32>()?))
}

/// A new invite code.
pub fn new_invite() -> Result<String, getrandom::Error> {
    Ok(format!("inv_{}", base32(&random::<20>()?)))
}

/// A new API token and its display prefix.
pub fn new_api_token() -> Result<(String, String), getrandom::Error> {
    let prefix: String = base32(&random::<8>()?).chars().take(12).collect();
    let secret = base32(&random::<20>()?);
    Ok((format!("{API_TOKEN_PREFIX}{prefix}_{secret}"), prefix))
}

/// True for text shaped like an API token (checked before any lookup).
pub fn looks_like_api_token(token: &str) -> bool {
    token.strip_prefix(API_TOKEN_PREFIX).is_some_and(|rest| {
        rest.len() == 45
            && rest.as_bytes()[12] == b'_'
            && rest
                .bytes()
                .enumerate()
                .all(|(i, b)| i == 12 || matches!(b, b'a'..=b'z' | b'2'..=b'7'))
    })
}

/// True for text shaped like a session id or an email link token (64 hex digits).
pub fn looks_like_hex_secret(secret: &str) -> bool {
    secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_matches_rfc_4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "my"),
            ("fo", "mzxq"),
            ("foo", "mzxw6"),
            ("foob", "mzxw6yq"),
            ("fooba", "mzxw6ytb"),
            ("foobar", "mzxw6ytboi"),
        ] {
            assert_eq!(base32(input.as_bytes()), expected, "{input:?}");
        }
    }

    #[test]
    fn secrets_have_their_shapes_and_differ() {
        let (token, prefix) = new_api_token().unwrap();
        assert!(looks_like_api_token(&token), "{token}");
        assert_eq!(prefix.len(), 12);
        assert!(token.starts_with(&format!("{API_TOKEN_PREFIX}{prefix}_")));
        assert_ne!(new_api_token().unwrap().0, token);
        assert!(!looks_like_api_token("xp_short"));
        assert!(!looks_like_api_token(&token.to_uppercase()));
        let session = new_hex_secret().unwrap();
        assert!(looks_like_hex_secret(&session));
        assert!(!looks_like_hex_secret(&session[1..]));
        assert_eq!(new_invite().unwrap().len(), 36);
        assert_ne!(digest("a"), digest("b"));
    }
}
