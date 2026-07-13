//! Dependencies shared by the background tasks (`spawn_task`, `teardown_task`, `ttl_reaper`,
//! `reconciliation`) — deliberately one bundle reused by all of them rather than a near-duplicate
//! struct per task, since they all operate on the same handful of ports.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::backends::ClusterBackend;
use crate::client_workers::ClientWorkers;
use crate::domain::ids::ClusterId;
use crate::domain::service_kind::ServiceKind;
use crate::ports::clock::Clock;
use crate::ports::privileged_exec::PrivilegedExecutor;
use crate::ports::repository::ClusterRepository;

/// Bundle of dependencies shared by every background task (`spawn_task`, `teardown_task`,
/// `ttl_reaper`, `reconciliation`). See the module docs above for why this is one shared struct
/// rather than a near-duplicate per task.
pub struct TaskDeps {
    /// Durable store of cluster rows.
    pub repository: Arc<dyn ClusterRepository>,
    /// Static mapping from each configured client to its own Unix account.
    pub client_workers: Arc<ClientWorkers>,
    /// Runs the closed set of privileged filesystem operations (prepare/wipe a worker's
    /// directory) as that worker's account.
    pub privileged_exec: Arc<dyn PrivilegedExecutor>,
    /// The registered backend implementation for each supported [`ServiceKind`], looked up by
    /// kind when a cluster is spawned or torn down.
    pub backends: HashMap<ServiceKind, Arc<dyn ClusterBackend>>,
    /// Source of the current time, injectable for deterministic tests.
    pub clock: Arc<dyn Clock>,
    /// Base directory under which each worker's own data directory lives (joined with the
    /// worker's name to get its actual path).
    pub worker_data_dir_base: PathBuf,
}

/// Tracks a `CancellationToken` per cluster with an in-flight `spawn_task`, so `DELETE` on a
/// still-`Spawning` cluster can signal it to stop rather than a fresh `teardown_task` racing
/// against it. Entries are registered when a spawn task starts and removed once it (however it
/// finished — success, failure, or cancellation) returns; a lingering entry only means "a spawn
/// is still in flight for this cluster."
#[derive(Default)]
pub struct TaskRegistry {
    /// One cancellation token per cluster currently being spawned.
    tokens: Mutex<HashMap<ClusterId, CancellationToken>>,
}

impl TaskRegistry {
    /// Builds an empty registry, with no clusters currently tracked.
    ///
    /// # Returns
    ///
    /// A fresh `TaskRegistry`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks and returns the token map. Never panics: a poisoned mutex (only possible if a prior
    /// critical section panicked, which our own code never does) is recovered from rather than
    /// propagated, since every critical section here is a single atomic map operation with no
    /// invariant that could be left broken halfway through.
    ///
    /// # Returns
    ///
    /// A guard giving exclusive access to the token map.
    fn tokens(&self) -> std::sync::MutexGuard<'_, HashMap<ClusterId, CancellationToken>> {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers a fresh cancellation token for `id`, replacing any prior one — a cluster only
    /// ever has one spawn in flight at a time, so this should only ever be called once per id
    /// before the matching [`TaskRegistry::unregister`].
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster whose in-flight spawn this token will cancel.
    ///
    /// # Returns
    ///
    /// The new token; the caller races it against its spawn work (e.g. via `tokio::select!`).
    pub fn register(&self, id: ClusterId) -> CancellationToken {
        let token = CancellationToken::new();
        self.tokens().insert(id, token.clone());
        token
    }

    /// Signals cancellation if `id` has a registered token; a no-op if it doesn't (e.g. the
    /// spawn already finished on its own before the delete arrived).
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster whose in-flight spawn (if any) should be cancelled.
    pub fn cancel(&self, id: &ClusterId) {
        if let Some(token) = self.tokens().get(id) {
            token.cancel();
        }
    }

    /// Removes `id`'s entry, if any — called once a spawn task has finished (however it
    /// finished), so a lingering entry always means "still in flight."
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster whose entry to remove.
    pub fn unregister(&self, id: &ClusterId) {
        self.tokens().remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::TaskRegistry;
    use crate::domain::ids::ClusterId;

    #[test]
    fn cancel_on_unregistered_id_is_a_no_op() {
        let registry = TaskRegistry::new();
        registry.cancel(&ClusterId::new(ulid::Ulid::nil()));
    }

    #[test]
    fn register_then_cancel_cancels_the_token() {
        let registry = TaskRegistry::new();
        let id = ClusterId::new(ulid::Ulid::nil());
        let token = registry.register(id);
        assert!(!token.is_cancelled());
        registry.cancel(&id);
        assert!(token.is_cancelled());
    }

    #[test]
    fn unregister_then_cancel_is_a_no_op() {
        let registry = TaskRegistry::new();
        let id = ClusterId::new(ulid::Ulid::nil());
        let token = registry.register(id);
        registry.unregister(&id);
        registry.cancel(&id);
        assert!(!token.is_cancelled());
    }
}
