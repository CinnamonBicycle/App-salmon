//! Client bearer secrets are high-entropy, machine-generated tokens (not human passphrases), so
//! a fast cryptographic hash (SHA-256) is the right tool — slow, memory-hard hashing (e.g.
//! argon2) exists to resist brute-forcing a *low*-entropy human password and would be pure
//! overhead here. Comparison is constant-time so a timing side-channel can't leak which prefix
//! bytes of a guessed secret were correct.

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// A SHA-256 digest of a client bearer secret, stored in config instead of the secret itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretHash(
    /// The raw 32-byte SHA-256 digest.
    [u8; 32],
);

impl SecretHash {
    /// Hashes `secret` with SHA-256.
    ///
    /// # Arguments
    ///
    /// - `secret`: the plaintext secret to hash (e.g. a client's bearer token).
    ///
    /// # Returns
    ///
    /// The `SecretHash` wrapping `secret`'s SHA-256 digest.
    #[must_use]
    pub fn of(secret: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(secret.as_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        Self(digest)
    }

    /// Checks whether `candidate` hashes to this digest, via a constant-time comparison so a
    /// timing side-channel can't leak which prefix bytes of a guessed secret were correct.
    ///
    /// # Arguments
    ///
    /// - `candidate`: the plaintext secret to check against this digest.
    ///
    /// # Returns
    ///
    /// `true` if `candidate`'s SHA-256 digest equals this one, `false` otherwise.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        let candidate_hash = Self::of(candidate);
        self.0.ct_eq(&candidate_hash.0).into()
    }

    /// Parses a `sha256:<64 lowercase hex chars>` string, as stored in config.
    ///
    /// # Arguments
    ///
    /// - `encoded`: the string to parse, expected in `sha256:<64 hex chars>` form.
    ///
    /// # Returns
    ///
    /// `Some(SecretHash)` if `encoded` is well-formed, `None` otherwise (missing `sha256:`
    /// prefix, wrong length, or non-hex characters).
    #[must_use]
    pub fn from_hex_with_prefix(encoded: &str) -> Option<Self> {
        let hex = encoded.strip_prefix("sha256:")?;
        if hex.len() != 64 {
            return None;
        }
        let parsed: Vec<u8> = hex
            .as_bytes()
            .chunks(2)
            .map(|chunk| {
                let byte_str = std::str::from_utf8(chunk).ok()?;
                u8::from_str_radix(byte_str, 16).ok()
            })
            .collect::<Option<Vec<u8>>>()?;
        let bytes: [u8; 32] = parsed.try_into().ok()?;
        Some(Self(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::SecretHash;

    #[test]
    fn matching_secret_matches() {
        let hash = SecretHash::of("correct-horse-battery-staple");
        assert!(hash.matches("correct-horse-battery-staple"));
    }

    #[test]
    fn wrong_secret_does_not_match() {
        let hash = SecretHash::of("correct-horse-battery-staple");
        assert!(!hash.matches("wrong-secret"));
    }

    #[test]
    fn empty_secret_does_not_match_nonempty_hash() {
        let hash = SecretHash::of("correct-horse-battery-staple");
        assert!(!hash.matches(""));
    }

    #[test]
    fn from_hex_with_prefix_round_trips() {
        use std::fmt::Write;

        let hash = SecretHash::of("some-secret");
        let mut hex = String::with_capacity(64);
        for byte in hash.0 {
            let _ = write!(hex, "{byte:02x}");
        }
        let encoded = format!("sha256:{hex}");
        let parsed = SecretHash::from_hex_with_prefix(&encoded).expect("valid encoding parses");
        assert_eq!(parsed, hash);
    }

    #[test]
    fn from_hex_with_prefix_rejects_missing_prefix() {
        assert!(SecretHash::from_hex_with_prefix("deadbeef").is_none());
    }

    #[test]
    fn from_hex_with_prefix_rejects_wrong_length() {
        assert!(SecretHash::from_hex_with_prefix("sha256:deadbeef").is_none());
    }

    #[test]
    fn from_hex_with_prefix_rejects_non_hex_characters() {
        let bogus = format!("sha256:{}", "z".repeat(64));
        assert!(SecretHash::from_hex_with_prefix(&bogus).is_none());
    }
}
