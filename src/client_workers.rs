//! Maps each configured client to its own Unix account — `app_salmon` uses `sudo -u <name>` to
//! prepare/wipe that account's per-cluster directory, and Docker's `--user <uid>:<gid>` to run
//! the cluster's container as that account. One account per client, not a shared pool: a client's
//! account is the one its clusters always run as, resolved once at startup from `config.toml`'s
//! `[[clients]]` entries (see `adapters::system_users`) and never reassigned at runtime.
//!
//! This has no I/O of its own — preparing/wiping a client's directory goes through the injected
//! `PrivilegedExecutor` at the call site (`service::spawn_task` / `service::teardown_task`), not
//! through this type. [`ClientWorkers`] only holds the static client -> account mapping, which is
//! why it's a concrete struct rather than a trait: there's nothing external to fake.
//!
//! Phase-1 security note (see `docs/DESIGN.md`): this is a file-ownership/attribution boundary,
//! not a container-escape boundary — the Docker daemon that actually runs containers still runs
//! as root either way. Per-client (rather than pooled) accounts additionally mean one client's
//! account never runs code on another client's behalf, so a capability eventually granted to one
//! client's account can't leak to a different client merely through account reuse — a real gap a
//! shared pool would have once clients need genuinely different capabilities, not just isolated
//! storage.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::domain::ids::{ClientId, WorkerUser};
use crate::ports::privileged_exec::PrivilegedExecError;

/// Computes the on-disk directory a cluster's container bind-mounts into: `worker`'s own
/// subdirectory under `base`, further scoped by `slot` so a client with more than one concurrent
/// cluster (up to `max_clusters_per_user`) gets a distinct directory per cluster rather than
/// sharing one across its account.
///
/// Scoped by a small, fixed slot number (`0..max_clusters_per_user`) rather than the cluster's own
/// id deliberately: the id is unbounded/unenumerable, which would force the `/etc/sudoers.d` rule
/// `scripts/setup-e2e-env.sh` writes to use a wildcard to match it — and at least one real `sudo`
/// implementation (`sudo-rs`, confirmed via `visudo -c` in this environment) rejects wildcards
/// embedded in command arguments outright (`syntax error: wildcards are not allowed in command
/// arguments`), so a wildcard-based rule simply fails to install. A slot number is bounded and
/// known ahead of time, so the sudoers rule can instead enumerate exactly `max_clusters_per_user`
/// literal, allowed paths per client — no wildcard needed at all.
///
/// # Arguments
///
/// - `base`: the configured base directory all client directories live under.
/// - `worker`: the client account whose directory to compute.
/// - `slot`: the cluster's assigned directory slot (see [`crate::domain::cluster::Cluster::slot`]).
///
/// # Returns
///
/// `base` joined with `worker`'s account name and `slot-<slot>`.
#[must_use]
pub fn worker_data_dir(base: &Path, worker: &WorkerUser, slot: u32) -> PathBuf {
    base.join(worker.as_str()).join(format!("slot-{slot}"))
}

/// Errors from [`ClientWorkers`] lookups or the privileged operations performed against a
/// resolved account.
#[derive(Debug, Error)]
pub enum ClientWorkerError {
    /// `client` has no configured Unix account — should never happen for a cluster's owner, since
    /// every owner was authenticated against the same client list this mapping was built from;
    /// surfaced as an error rather than a panic in case the two ever drift.
    #[error("no unix account configured for client {client}")]
    UnknownClient {
        /// The client whose account lookup failed.
        client: ClientId,
    },
    /// The `PrivilegedExecutor` call to create/`chown` a cluster's directory failed.
    #[error("failed to prepare client directory: {0}")]
    Prepare(#[source] PrivilegedExecError),
    /// The `PrivilegedExecutor` call to wipe a cluster's directory on teardown failed.
    #[error("failed to wipe client directory: {0}")]
    Wipe(#[source] PrivilegedExecError),
}

/// The static mapping from each configured client to its own Unix account, resolved once at
/// startup (see `adapters::system_users`) and never reassigned — see the module docs for why this
/// replaces a shared worker pool.
pub struct ClientWorkers(HashMap<ClientId, WorkerUser>);

impl ClientWorkers {
    /// Wraps an already-resolved client -> account mapping.
    ///
    /// # Arguments
    ///
    /// - `accounts`: one entry per configured client, resolved from `config.toml` and
    ///   `/etc/passwd` at startup.
    ///
    /// # Returns
    ///
    /// The constructed `ClientWorkers`.
    #[must_use]
    pub fn new(accounts: HashMap<ClientId, WorkerUser>) -> Self {
        Self(accounts)
    }

    /// Looks up the Unix account a given client's clusters run as.
    ///
    /// # Arguments
    ///
    /// - `client`: the client to look up.
    ///
    /// # Returns
    ///
    /// A clone of `client`'s configured [`WorkerUser`].
    ///
    /// # Errors
    ///
    /// Returns [`ClientWorkerError::UnknownClient`] if `client` has no configured account — see
    /// that variant's docs for why this should never happen in practice.
    pub fn get(&self, client: &ClientId) -> Result<WorkerUser, ClientWorkerError> {
        self.0
            .get(client)
            .cloned()
            .ok_or_else(|| ClientWorkerError::UnknownClient {
                client: client.clone(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientWorkerError, ClientWorkers};
    use crate::domain::ids::{ClientId, WorkerUser};
    use std::collections::HashMap;

    #[test]
    fn get_returns_the_configured_account() {
        let client = ClientId::new("agent");
        let worker = WorkerUser::new("agent", 2000, 2000);
        let workers = ClientWorkers::new(HashMap::from([(client.clone(), worker.clone())]));
        assert_eq!(workers.get(&client).expect("configured"), worker);
    }

    #[test]
    fn get_errors_for_an_unconfigured_client() {
        let workers = ClientWorkers::new(HashMap::new());
        let err = workers
            .get(&ClientId::new("nobody"))
            .expect_err("not configured");
        assert!(
            matches!(err, ClientWorkerError::UnknownClient { client } if client == ClientId::new("nobody"))
        );
    }

    #[test]
    fn worker_data_dir_joins_base_worker_and_slot() {
        let worker = WorkerUser::new("agent", 2007, 2007);
        let path = super::worker_data_dir(
            std::path::Path::new("/var/lib/app_salmon/workers"),
            &worker,
            1,
        );
        assert_eq!(
            path,
            std::path::PathBuf::from("/var/lib/app_salmon/workers/agent/slot-1")
        );
    }

    #[test]
    fn client_worker_error_display_messages() {
        assert_eq!(
            ClientWorkerError::UnknownClient {
                client: ClientId::new("agent")
            }
            .to_string(),
            "no unix account configured for client agent"
        );
    }
}
