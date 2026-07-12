use rand::RngExt;
use rand::distr::Alphanumeric;
use ulid::Ulid;

use crate::domain::ids::ClusterId;
use crate::ports::secrets::SecretGenerator;

/// Real [`SecretGenerator`] implementation backed by a CSPRNG. Zero-sized; the only production
/// implementation of `SecretGenerator` (tests use a fake with deterministic output instead).
#[derive(Debug, Default, Clone, Copy)]
pub struct RandSecretGenerator;

impl SecretGenerator for RandSecretGenerator {
    /// Generates a new, random cluster identifier.
    ///
    /// # Returns
    ///
    /// A [`ClusterId`] wrapping a freshly-generated random ULID.
    fn cluster_id(&self) -> ClusterId {
        ClusterId::new(Ulid::r#gen())
    }

    /// Generates a random alphanumeric password.
    ///
    /// # Arguments
    ///
    /// - `len`: the number of characters the generated password should have.
    ///
    /// # Returns
    ///
    /// A random string of exactly `len` alphanumeric (`[A-Za-z0-9]`) characters, drawn from the
    /// system CSPRNG.
    fn db_password(&self, len: usize) -> String {
        rand::rng()
            .sample_iter(&Alphanumeric)
            .take(len)
            .map(char::from)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::RandSecretGenerator;
    use crate::ports::secrets::SecretGenerator;

    #[test]
    fn db_password_has_requested_length() {
        let generator = RandSecretGenerator;
        assert_eq!(generator.db_password(24).len(), 24);
    }

    #[test]
    fn db_password_is_alphanumeric() {
        let generator = RandSecretGenerator;
        let password = generator.db_password(64);
        assert!(password.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn db_password_calls_produce_different_values() {
        let generator = RandSecretGenerator;
        let a = generator.db_password(32);
        let b = generator.db_password(32);
        assert_ne!(a, b);
    }

    #[test]
    fn cluster_id_calls_produce_different_values() {
        let generator = RandSecretGenerator;
        assert_ne!(generator.cluster_id(), generator.cluster_id());
    }
}
