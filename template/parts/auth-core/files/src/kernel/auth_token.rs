//! Token, code and digest primitives shared by every auth sample: session cookies, emailed links,
//! one-time codes, recovery codes and trusted-device cookies all come from here.
//!
//! GUIDE: .offrails/guides/auth.md#tokens-kernelsessionrs
//!
//! Two digests, for two kinds of secret:
//! - `digest(token)`: SHA-256 of a 256-bit random token. Guessing the token from a leaked digest
//!   is as hard as guessing the token, so no key is needed.
//! - `keyed_digest(purpose, code)`: HMAC-SHA-256 under `APP_SECRET_KEY` for **short** secrets
//!   (6-digit codes, recovery codes). A plain SHA-256 of a 6-digit code is reversed by trying all
//!   10^6 codes; with the key, a database leak alone reveals nothing.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

use crate::kernel::webhook;

/// 32 random bytes, base64url without padding (43 characters).
// SECURITY: 256 bits from the OS CSPRNG; a shorter or non-crypto token can be guessed or
// predicted (test: tokens_are_long_unique_and_url_safe)
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Lowercase hex SHA-256. This, never the raw token, is what the database stores.
// SECURITY: a leaked table then holds no usable session, link or reset token.
// INVARIANT: only digests are stored (test: only_token_digests_are_stored)
pub fn digest(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

/// HMAC-SHA-256 of a short secret, bound to its purpose ("login_code", "recovery_code", …), so
/// a digest from one table can't be replayed against another.
// SECURITY: keyed so a database leak can't brute-force 6-digit codes offline (10^6 tries)
// (test: keyed_digests_depend_on_key_and_purpose)
pub fn keyed_digest(key: &[u8], purpose: &str, secret: &str) -> String {
    hex(&webhook::hmac_sha256(
        key,
        format!("{purpose}:{secret}").as_bytes(),
    ))
}

/// Compares two byte strings without an early exit, so the time taken doesn't reveal how many
/// leading bytes matched. Both sides are hashed first, so the length doesn't leak either.
// SECURITY: a `==` on secrets leaks the matching prefix through timing (byte-by-byte guessing).
// INVARIANT: every token and code comparison goes through here or a digest lookup
// (test: constant_time_eq_compares_content)
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let (da, db) = (Sha256::digest(a), Sha256::digest(b));
    da.iter()
        .zip(db.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// A uniformly random numeric code, zero-padded (`"042917"`).
// SECURITY: `random_range` is unbiased; `% 10^n` on a random u32 would favour low codes
// (test: codes_are_zero_padded_digits)
pub fn generate_code(digits: u32) -> String {
    assert!((1..=9).contains(&digits), "code length must be 1..=9 digits");
    let max = 10u32.pow(digits);
    let n: u32 = rand::random_range(0..max);
    format!("{n:0width$}", width = digits as usize)
}

/// Keeps only ASCII digits: "123 456", "123-456" and pasted codes with spaces all work.
pub fn digits_only(input: &str) -> String {
    input.chars().filter(char::is_ascii_digit).collect()
}

fn hex(bytes: &[u8]) -> String {
    // WHY: sha2 0.11's output doesn't implement LowerHex, so encode byte by byte.
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_unique_and_url_safe() {
        let (a, b) = (generate_token(), generate_token());
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn digests_are_stable_hex() {
        assert_eq!(digest("hello").len(), 64);
        assert_eq!(digest("hello"), digest("hello"));
        assert_ne!(digest("hello"), digest("hellp"));
    }

    #[test]
    fn keyed_digests_depend_on_key_and_purpose() {
        let d = keyed_digest(b"key-one", "login_code", "123456");
        assert_eq!(d, keyed_digest(b"key-one", "login_code", "123456"));
        assert_ne!(d, keyed_digest(b"key-two", "login_code", "123456"));
        assert_ne!(d, keyed_digest(b"key-one", "recovery_code", "123456"));
        assert_ne!(d, digest("123456"));
    }

    #[test]
    fn constant_time_eq_compares_content() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret!"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn codes_are_zero_padded_digits() {
        for _ in 0..200 {
            let c = generate_code(6);
            assert_eq!(c.len(), 6);
            assert!(c.chars().all(|ch| ch.is_ascii_digit()));
        }
        assert_eq!(digits_only(" 123-456 "), "123456");
    }
}
