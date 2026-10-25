//! HMAC-SHA256 (RFC 2104) and a constant-time comparison, for the kernel and the parts that need
//! a keyed digest: webhook signatures (`kernel::webhook`), short-code digests (auth-core).
//!
//! WHY: written out rather than adding the `hmac` crate to every app: it is twenty lines over
//! `sha2` (already a dependency), it can't fail on any key length, and the RFC 4231 vectors below
//! pin it, including the hash-the-key path for keys longer than a block.

use sha2::{Digest, Sha256};

/// HMAC-SHA256 of `data` under `secret`.
pub fn hmac_sha256(secret: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key = [0u8; BLOCK];
    if secret.len() > BLOCK {
        key[..32].copy_from_slice(&Sha256::digest(secret));
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }
    let mut inner = Vec::with_capacity(BLOCK + data.len());
    inner.extend(key.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(data);
    let inner_hash = Sha256::digest(&inner);
    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend(key.iter().map(|b| b ^ 0x5c));
    outer.extend_from_slice(&inner_hash);
    Sha256::digest(&outer).into()
}

/// `a == b`, without an early exit: the time taken doesn't reveal how many leading bytes matched.
/// Different lengths are unequal (lengths aren't secret here: digests have fixed sizes).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Strict hex: an even number of ASCII hex digits, nothing else (no sign, no `0x`, no spaces).
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() || !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231() {
        // Test cases 1, 2, 6 (a 131-byte key: hashed first) and 7 (long key and long data).
        let cases: [(Vec<u8>, &[u8], &str); 4] = [
            (
                vec![0x0b; 20],
                b"Hi There",
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe".to_vec(),
                b"what do ya want for nothing?",
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                vec![0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First",
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
            (
                vec![0xaa; 131],
                b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.",
                "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2",
            ),
        ];
        for (key, data, want) in cases {
            assert_eq!(hex(&hmac_sha256(&key, data)), want);
        }
    }

    #[test]
    fn comparison_and_hex_are_strict() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert_eq!(from_hex("00ff"), Some(vec![0, 255]));
        assert_eq!(from_hex("00FF"), Some(vec![0, 255]));
        for bad in ["", "0", "zz", "+f", "0x", " 0", "é0"] {
            assert_eq!(from_hex(bad), None, "{bad:?}");
        }
    }
}
