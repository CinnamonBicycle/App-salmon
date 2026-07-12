//! Bearer-token authentication: `Authorization: Bearer <client_name>:<secret>`.
//!
//! The client name is looked up directly (it isn't secret — only the paired secret proves
//! ownership of the name), then the secret is checked via constant-time hash comparison. See
//! `docs/DESIGN.md` for why this phase uses plain bearer tokens over TCP rather than TLS/mTLS.

pub mod hashing;

use std::collections::HashMap;

use thiserror::Error;

use crate::domain::ids::ClientId;
use hashing::SecretHash;

/// Why [`ClientRegistry::authenticate`] rejected a request.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuthError {
    /// No `Authorization` header was present at all.
    #[error("missing Authorization header")]
    MissingCredentials,
    /// The header was present but not in `Bearer <name>:<secret>` form (missing `Bearer `
    /// prefix, missing `:` separator, or an empty name/secret either side of it).
    #[error("malformed Authorization header")]
    MalformedHeader,
    /// `<name>` doesn't match any client in the registry.
    #[error("unknown client")]
    UnknownClient,
    /// `<name>` is known, but `<secret>` doesn't match its stored hash.
    #[error("credential mismatch")]
    InvalidSecret,
}

/// The set of clients allowed to call the API, keyed by client name, each mapped to the hash of
/// its bearer secret.
pub struct ClientRegistry {
    /// Registered clients: client id to the hash of its secret.
    clients: HashMap<ClientId, SecretHash>,
}

impl ClientRegistry {
    /// Builds a registry from an already-assembled client-to-secret-hash map.
    ///
    /// # Arguments
    ///
    /// - `clients`: the registered clients, keyed by [`ClientId`], each mapped to the
    ///   [`SecretHash`] of its bearer secret.
    ///
    /// # Returns
    ///
    /// A `ClientRegistry` serving lookups against `clients`.
    #[must_use]
    pub fn new(clients: HashMap<ClientId, SecretHash>) -> Self {
        Self { clients }
    }

    /// Validates an `Authorization` header value against this registry.
    ///
    /// # Arguments
    ///
    /// - `header_value`: the raw `Authorization` header value, if present, expected in
    ///   `Bearer <client_name>:<secret>` form.
    ///
    /// # Returns
    ///
    /// The authenticated caller's [`ClientId`] if `header_value` carries valid credentials for a
    /// registered client.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::MissingCredentials`] if `header_value` is `None`,
    /// [`AuthError::MalformedHeader`] if it isn't `Bearer <name>:<secret>`,
    /// [`AuthError::UnknownClient`] if `<name>` has no registry entry, or
    /// [`AuthError::InvalidSecret`] if the secret doesn't match that entry's stored hash.
    pub fn authenticate(&self, header_value: Option<&str>) -> Result<ClientId, AuthError> {
        let header_value = header_value.ok_or(AuthError::MissingCredentials)?;
        let credentials = header_value
            .strip_prefix("Bearer ")
            .ok_or(AuthError::MalformedHeader)?;
        let (name, secret) = credentials
            .split_once(':')
            .ok_or(AuthError::MalformedHeader)?;
        if name.is_empty() || secret.is_empty() {
            return Err(AuthError::MalformedHeader);
        }
        let client_id = ClientId::new(name);
        let expected_hash = self
            .clients
            .get(&client_id)
            .ok_or(AuthError::UnknownClient)?;
        if expected_hash.matches(secret) {
            Ok(client_id)
        } else {
            Err(AuthError::InvalidSecret)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthError, ClientRegistry};
    use crate::domain::ids::ClientId;
    use hashing::SecretHash;

    use super::hashing;

    fn registry() -> ClientRegistry {
        let mut clients = std::collections::HashMap::new();
        clients.insert(ClientId::new("openbrain-agent"), SecretHash::of("s3cret"));
        ClientRegistry::new(clients)
    }

    #[test]
    fn missing_header_is_missing_credentials() {
        let err = registry().authenticate(None).expect_err("missing header");
        assert_eq!(err, AuthError::MissingCredentials);
    }

    #[test]
    fn header_without_bearer_prefix_is_malformed() {
        let err = registry()
            .authenticate(Some("openbrain-agent:s3cret"))
            .expect_err("malformed");
        assert_eq!(err, AuthError::MalformedHeader);
    }

    #[test]
    fn header_without_colon_separator_is_malformed() {
        let err = registry()
            .authenticate(Some("Bearer nocolonhere"))
            .expect_err("malformed");
        assert_eq!(err, AuthError::MalformedHeader);
    }

    #[test]
    fn header_with_empty_name_is_malformed() {
        let err = registry()
            .authenticate(Some("Bearer :s3cret"))
            .expect_err("malformed");
        assert_eq!(err, AuthError::MalformedHeader);
    }

    #[test]
    fn header_with_empty_secret_is_malformed() {
        let err = registry()
            .authenticate(Some("Bearer openbrain-agent:"))
            .expect_err("malformed");
        assert_eq!(err, AuthError::MalformedHeader);
    }

    #[test]
    fn unknown_client_name_is_rejected() {
        let err = registry()
            .authenticate(Some("Bearer nobody:s3cret"))
            .expect_err("unknown client");
        assert_eq!(err, AuthError::UnknownClient);
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let err = registry()
            .authenticate(Some("Bearer openbrain-agent:wrong"))
            .expect_err("invalid secret");
        assert_eq!(err, AuthError::InvalidSecret);
    }

    #[test]
    fn correct_credentials_authenticate() {
        let client_id = registry()
            .authenticate(Some("Bearer openbrain-agent:s3cret"))
            .expect("valid credentials");
        assert_eq!(client_id, ClientId::new("openbrain-agent"));
    }
}
