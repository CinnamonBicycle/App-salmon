//! Strongly typed identifiers so a `ClusterId` and a `ClientId` can never be swapped by accident
//! at a call site — the compiler rejects it, rather than a bug surfacing at runtime as one
//! client seeing another client's cluster.

use std::fmt;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Sortable (by creation time) unique identifier for a cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClusterId(Ulid);

impl ClusterId {
    /// Wraps an existing [`Ulid`] as a [`ClusterId`].
    ///
    /// # Arguments
    ///
    /// - `ulid`: the underlying identifier to wrap.
    ///
    /// # Returns
    ///
    /// The wrapped id.
    #[must_use]
    pub fn new(ulid: Ulid) -> Self {
        Self(ulid)
    }

    /// Returns the underlying [`Ulid`], e.g. to compare creation order or format it directly.
    ///
    /// # Returns
    ///
    /// A copy of the wrapped [`Ulid`].
    #[must_use]
    pub fn as_ulid(&self) -> Ulid {
        self.0
    }
}

impl fmt::Display for ClusterId {
    /// Writes the id in its canonical ULID string form.
    ///
    /// # Arguments
    ///
    /// - `f`: the formatter to write to.
    ///
    /// # Returns
    ///
    /// `Ok(())` on success, or the formatter's write error.
    ///
    /// # Errors
    ///
    /// Returns an error only if writing to `f` itself fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A [`ClusterId`] failed to parse from its string form (see [`ClusterId`]'s `FromStr` impl).
#[derive(Debug, thiserror::Error)]
#[error("invalid cluster id: {0}")]
pub struct ParseClusterIdError(
    /// The underlying ULID decode failure.
    #[from]
    ulid::DecodeError,
);

impl std::str::FromStr for ClusterId {
    /// A malformed input string fails with [`ParseClusterIdError`], wrapping the underlying
    /// ULID decode failure.
    type Err = ParseClusterIdError;

    /// Parses a canonical ULID string (as produced by [`ClusterId`]'s `Display` impl) back into a
    /// [`ClusterId`].
    ///
    /// # Arguments
    ///
    /// - `s`: the string to parse.
    ///
    /// # Returns
    ///
    /// The parsed [`ClusterId`].
    ///
    /// # Errors
    ///
    /// Returns [`ParseClusterIdError`] if `s` is not a valid ULID string.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Ulid::from_string(s)?))
    }
}

/// The name a client account authenticates as (matches a `[[clients]]` entry in config).
/// Not secret by itself — the paired bearer secret is what proves the caller owns the name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientId(String);

impl ClientId {
    /// Wraps a client account name as a [`ClientId`].
    ///
    /// # Arguments
    ///
    /// - `name`: the client account name, matching a `[[clients]]` entry in config.
    ///
    /// # Returns
    ///
    /// The wrapped id.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Borrows the underlying client account name as a plain string slice.
    ///
    /// # Returns
    ///
    /// The raw client account name this id wraps.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClientId {
    /// Writes the underlying client account name, unmodified.
    ///
    /// # Arguments
    ///
    /// - `f`: the formatter to write to.
    ///
    /// # Returns
    ///
    /// `Ok(())` on success, or the formatter's write error.
    ///
    /// # Errors
    ///
    /// Returns an error only if writing to `f` itself fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A worker account allocated from the pool (e.g. `salmon-worker-03`) — `app_salmon` uses
/// `sudo -u <name>` to prepare/wipe that worker's per-cluster directory, and Docker's
/// `--user <uid>:<gid>` to run the cluster's container as that account. Both the name (for sudo)
/// and the numeric ids (for Docker, which can't resolve a host-only account name inside the
/// container) are needed, so this carries all three rather than just the name — see
/// `adapters::system_users` for how they're resolved from the account name at startup.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkerUser {
    /// The Unix account name, e.g. `salmon-worker-03` — used with `sudo -u <name>`.
    name: String,
    /// The account's numeric user id — used with Docker's `--user <uid>:<gid>`.
    uid: u32,
    /// The account's numeric group id — used with Docker's `--user <uid>:<gid>`.
    gid: u32,
}

impl WorkerUser {
    /// Constructs a [`WorkerUser`] from an account name and its resolved uid/gid (see
    /// `adapters::system_users`).
    ///
    /// # Arguments
    ///
    /// - `name`: the Unix account name, e.g. `salmon-worker-03`.
    /// - `uid`: the account's numeric user id.
    /// - `gid`: the account's numeric group id.
    ///
    /// # Returns
    ///
    /// The constructed [`WorkerUser`].
    #[must_use]
    pub fn new(name: impl Into<String>, uid: u32, gid: u32) -> Self {
        Self {
            name: name.into(),
            uid,
            gid,
        }
    }

    /// Borrows the underlying Unix account name as a plain string slice.
    ///
    /// # Returns
    ///
    /// The account name, e.g. `salmon-worker-03`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.name
    }

    /// Returns the account's numeric user id.
    ///
    /// # Returns
    ///
    /// The uid, for use with Docker's `--user uid:gid`.
    #[must_use]
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// Returns the account's numeric group id.
    ///
    /// # Returns
    ///
    /// The gid, for use with Docker's `--user uid:gid`.
    #[must_use]
    pub fn gid(&self) -> u32 {
        self.gid
    }
}

impl fmt::Display for WorkerUser {
    /// Writes the underlying Unix account name, unmodified.
    ///
    /// # Arguments
    ///
    /// - `f`: the formatter to write to.
    ///
    /// # Returns
    ///
    /// `Ok(())` on success, or the formatter's write error.
    ///
    /// # Errors
    ///
    /// Returns an error only if writing to `f` itself fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientId, ClusterId};
    use ulid::Ulid;

    #[test]
    fn cluster_id_round_trips_through_display() {
        let ulid = Ulid::r#gen();
        let id = ClusterId::new(ulid);
        assert_eq!(id.to_string(), ulid.to_string());
        assert_eq!(id.as_ulid(), ulid);
    }

    #[test]
    fn cluster_id_serde_round_trip() {
        let id = ClusterId::new(Ulid::r#gen());
        let json = serde_json::to_string(&id).expect("serialize cluster id");
        let back: ClusterId = serde_json::from_str(&json).expect("deserialize cluster id");
        assert_eq!(id, back);
    }

    #[test]
    fn cluster_ids_order_by_creation_time() {
        let first = ClusterId::new(Ulid::r#gen());
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = ClusterId::new(Ulid::r#gen());
        assert!(first < second);
    }

    #[test]
    fn client_id_display_and_as_str() {
        let id = ClientId::new("openbrain-agent");
        assert_eq!(id.to_string(), "openbrain-agent");
        assert_eq!(id.as_str(), "openbrain-agent");
    }

    #[test]
    fn client_id_equality() {
        assert_eq!(ClientId::new("a"), ClientId::new("a"));
        assert_ne!(ClientId::new("a"), ClientId::new("b"));
    }

    #[test]
    fn worker_user_display_and_as_str() {
        let worker = super::WorkerUser::new("salmon-worker-00", 2000, 2000);
        assert_eq!(worker.to_string(), "salmon-worker-00");
        assert_eq!(worker.as_str(), "salmon-worker-00");
    }

    #[test]
    fn worker_user_equality() {
        assert_eq!(
            super::WorkerUser::new("salmon-worker-00", 2000, 2000),
            super::WorkerUser::new("salmon-worker-00", 2000, 2000)
        );
        assert_ne!(
            super::WorkerUser::new("salmon-worker-00", 2000, 2000),
            super::WorkerUser::new("salmon-worker-01", 2001, 2001)
        );
    }
}
