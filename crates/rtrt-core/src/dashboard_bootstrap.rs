//! Short-lived, domain-separated dashboard browser bootstrap credentials.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;

const VERSION: u8 = 1;
const DOMAIN: &[u8] = b"rtrt-dashboard-browser-bootstrap\0v1\0";
const NONCE_LEN: usize = 16;
const MAC_LEN: usize = 32;
const WIRE_LEN: usize = 1 + 8 + 8 + NONCE_LEN + MAC_LEN;
pub const ENCODED_LEN: usize = 87;
pub const MAX_TTL_SECS: u64 = 60;
pub const MAX_FUTURE_SKEW_SECS: u64 = 5;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct VerifiedBootstrap {
    pub nonce: [u8; NONCE_LEN],
    pub expires_at: u64,
}

impl std::fmt::Debug for VerifiedBootstrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedBootstrap")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapError {
    InvalidToken,
    Randomness,
    InvalidCredential,
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidToken => "dashboard token must be 256-bit hexadecimal",
            Self::Randomness => "operating-system randomness unavailable",
            Self::InvalidCredential => "bootstrap credential rejected",
        })
    }
}

impl std::error::Error for BootstrapError {}

pub fn issue(token: &str, now: u64) -> Result<String, BootstrapError> {
    let mut nonce = [0_u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|_| BootstrapError::Randomness)?;
    issue_with_nonce(token, now, MAX_TTL_SECS, nonce)
}

pub fn issue_with_nonce(
    token: &str,
    issued_at: u64,
    ttl_secs: u64,
    nonce: [u8; NONCE_LEN],
) -> Result<String, BootstrapError> {
    if ttl_secs == 0 || ttl_secs > MAX_TTL_SECS {
        return Err(BootstrapError::InvalidCredential);
    }
    let expires_at = issued_at
        .checked_add(ttl_secs)
        .ok_or(BootstrapError::InvalidCredential)?;
    let key = decode_token(token)?;
    let mut wire = [0_u8; WIRE_LEN];
    wire[0] = VERSION;
    wire[1..9].copy_from_slice(&issued_at.to_be_bytes());
    wire[9..17].copy_from_slice(&expires_at.to_be_bytes());
    wire[17..33].copy_from_slice(&nonce);
    let tag = sign(&key, &wire[..33]);
    wire[33..].copy_from_slice(&tag);
    let encoded = URL_SAFE_NO_PAD.encode(wire);
    debug_assert_eq!(encoded.len(), ENCODED_LEN);
    Ok(encoded)
}

pub fn verify(
    token: &str,
    credential: &str,
    now: u64,
) -> Result<VerifiedBootstrap, BootstrapError> {
    if credential.len() != ENCODED_LEN {
        return Err(BootstrapError::InvalidCredential);
    }
    let key = decode_token(token).map_err(|_| BootstrapError::InvalidCredential)?;
    let decoded = URL_SAFE_NO_PAD
        .decode(credential)
        .map_err(|_| BootstrapError::InvalidCredential)?;
    if decoded.len() != WIRE_LEN || decoded[0] != VERSION {
        return Err(BootstrapError::InvalidCredential);
    }
    let issued_at = u64::from_be_bytes(decoded[1..9].try_into().unwrap_or_default());
    let expires_at = u64::from_be_bytes(decoded[9..17].try_into().unwrap_or_default());
    if expires_at <= issued_at
        || expires_at.saturating_sub(issued_at) > MAX_TTL_SECS
        || now > expires_at
        || issued_at > now.saturating_add(MAX_FUTURE_SKEW_SECS)
    {
        return Err(BootstrapError::InvalidCredential);
    }
    let mut mac =
        HmacSha256::new_from_slice(&key).map_err(|_| BootstrapError::InvalidCredential)?;
    mac.update(DOMAIN);
    mac.update(&decoded[..33]);
    mac.verify_slice(&decoded[33..])
        .map_err(|_| BootstrapError::InvalidCredential)?;
    let mut nonce = [0_u8; NONCE_LEN];
    nonce.copy_from_slice(&decoded[17..33]);
    Ok(VerifiedBootstrap { nonce, expires_at })
}

fn decode_token(token: &str) -> Result<[u8; 32], BootstrapError> {
    if token.len() != 64 {
        return Err(BootstrapError::InvalidToken);
    }
    let mut key = [0_u8; 32];
    for (index, pair) in token.as_bytes().chunks_exact(2).enumerate() {
        key[index] = (hex(pair[0])? << 4) | hex(pair[1])?;
    }
    Ok(key)
}

fn hex(byte: u8) -> Result<u8, BootstrapError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(BootstrapError::InvalidToken),
    }
}

fn sign(key: &[u8; 32], message: &[u8]) -> [u8; MAC_LEN] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts every key length");
    mac.update(DOMAIN);
    mac.update(message);
    mac.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER: &str = "1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn fixture(issued: u64) -> String {
        issue_with_nonce(TOKEN, issued, 60, [7; 16]).unwrap()
    }

    #[test]
    fn issue_verify_has_fixed_url_fragment_safe_format() {
        let credential = fixture(1_000);
        assert_eq!(credential.len(), ENCODED_LEN);
        assert!(
            credential
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        );
        assert_eq!(verify(TOKEN, &credential, 1_030).unwrap().nonce, [7; 16]);
    }

    #[test]
    fn rejects_expired_future_tampered_and_wrong_token() {
        let credential = fixture(1_000);
        assert!(verify(TOKEN, &credential, 1_061).is_err());
        assert!(verify(TOKEN, &credential, 994).is_err());
        assert!(verify(OTHER, &credential, 1_010).is_err());
        let mut tampered = credential.into_bytes();
        tampered[40] = if tampered[40] == b'A' { b'B' } else { b'A' };
        assert!(verify(TOKEN, std::str::from_utf8(&tampered).unwrap(), 1_010).is_err());
    }

    #[test]
    fn rejects_malformed_oversized_and_invalid_lifetime() {
        assert!(verify(TOKEN, "x", 1_000).is_err());
        assert!(verify(TOKEN, &"A".repeat(ENCODED_LEN + 1), 1_000).is_err());
        assert!(issue_with_nonce(TOKEN, 1_000, 61, [0; 16]).is_err());
        assert!(issue_with_nonce("not-a-token", 1_000, 60, [0; 16]).is_err());
    }
}
