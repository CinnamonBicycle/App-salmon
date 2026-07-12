//! CSPRNG-backed ID/secret generation as a trait, so tests get deterministic, reproducible
//! values instead of real random data.

use crate::domain::ids::ClusterId;

pub trait SecretGenerator: Send + Sync {
    /// Generates a fresh, random cluster identifier.
    ///
    /// # Returns
    ///
    /// A new [`ClusterId`], unique with overwhelming probability.
    fn cluster_id(&self) -> ClusterId;

    /// A high-entropy random string suitable for a single-use, machine-generated database
    /// password. Not a human passphrase, so no slow hashing is needed to store it — see
    /// `docs/DESIGN.md` for why.
    ///
    /// # Arguments
    ///
    /// - `len`: the length, in characters, of the password to generate.
    ///
    /// # Returns
    ///
    /// A freshly generated random password of length `len`.
    fn db_password(&self, len: usize) -> String;
}
