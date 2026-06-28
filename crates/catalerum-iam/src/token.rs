//! Opaque token generation (SOUL §18).
//!
//! Session and one-time login tokens are random, URL-safe, high-entropy opaque
//! strings — never structured or guessable. We draw 32 bytes (256 bits) from
//! the thread RNG and encode them with an unpadded URL-safe base64 alphabet so
//! the token drops straight into a query string (`?token=…`).

use rand::Rng;
use sha2::{Digest, Sha256};

/// Default token entropy in bytes (256 bits).
pub const TOKEN_BYTES: usize = 32;

/// Hash a raw opaque token for storage (SOUL §18). The DB only ever sees this
/// hash, never the plaintext token handed to the caller.
///
/// SHA-256 is sufficient here: the input is already a 256-bit high-entropy
/// random token, so it is not brute-forceable and needs no salt/KDF — the hash
/// only prevents a database leak from yielding live bearer tokens. The result
/// is lowercase hex (64 chars).
#[must_use]
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Generate a fresh opaque token with the default 256 bits of entropy.
///
/// The result is URL-safe (alphabet `A–Z a–z 0–9 - _`, no padding) and stable
/// for direct use in a magic-link query string or an `Authorization: Bearer`
/// header.
#[must_use]
pub fn generate() -> String {
    generate_bytes(TOKEN_BYTES)
}

/// Generate an opaque token with `n` bytes of entropy.
///
/// Panics never; `n == 0` yields an empty string (callers should use
/// [`TOKEN_BYTES`]).
#[must_use]
pub fn generate_bytes(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    encode_url_safe(&buf)
}

/// Minimal dependency-free URL-safe base64 (no padding). Kept local so the
/// crate needs no extra base64 dependency.
fn encode_url_safe(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_and_url_safe() {
        let a = generate();
        let b = generate();
        assert_ne!(a, b, "tokens must not collide");
        // 32 bytes → 43 base64 chars (no padding).
        assert_eq!(a.len(), 43);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn encode_matches_known_vector() {
        // "foobar" → URL-safe base64 (no padding) = "Zm9vYmFy".
        assert_eq!(encode_url_safe(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode_url_safe(b"f"), "Zg");
        assert_eq!(encode_url_safe(b"fo"), "Zm8");
    }

    #[test]
    fn hash_token_is_stable_hex_sha256() {
        // SHA-256("abc") — a well-known test vector — as lowercase hex.
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Deterministic and 64 hex chars.
        let h = hash_token("some-opaque-token");
        assert_eq!(h, hash_token("some-opaque-token"));
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        // Distinct inputs hash distinctly.
        assert_ne!(hash_token("a"), hash_token("b"));
    }
}
