//! Durable storage for cluster state, so `GET /clusters` and reconciliation reflect reality
//! after a restart. The real adapter (`adapters::sqlite_repository`) is SQLite-backed; unit
//! tests use `InMemoryClusterRepository`.

use async_trait::async_trait;
use thiserror::Error;

use crate::domain::cluster::{Cluster, ClusterState};
use crate::domain::ids::{ClientId, ClusterId, WorkerUser};

#[cfg(test)]
use mockall::automock;

#[derive(Debug, Error)]
pub enum RepositoryError {
    /// The underlying database driver returned an error.
    #[error("database error: {0}")]
    Db(
        /// The underlying `rusqlite` error.
        #[source]
        rusqlite::Error,
    ),
    /// Converting a `Cluster`/`ClusterState` to or from its persisted JSON representation failed.
    #[error("failed to (de)serialize persisted cluster state: {0}")]
    Serde(
        /// The underlying `serde_json` error.
        #[source]
        serde_json::Error,
    ),
    /// Running the schema migrations at startup failed.
    #[error("migration failed: {0}")]
    Migration(
        /// A description of what went wrong.
        String,
    ),
    /// The blocking task running a `SQLite` operation panicked or was cancelled. Our own code
    /// never panics, so in practice this only fires if the process is shutting down mid-query.
    #[error("database task failed to complete: {0}")]
    TaskJoin(
        /// A description of why the task didn't complete.
        String,
    ),
    /// A stored row's `id` column or JSON payload didn't parse the way we expect. Since we're
    /// the only writer, this should only be reachable via external tampering or a prior bug —
    /// still handled as a `Result`, not a panic, since it's data read back from a system
    /// boundary (disk).
    #[error("corrupt row in cluster storage: {0}")]
    CorruptRow(
        /// A description of what was wrong with the row.
        String,
    ),
}

/// Outcome of the atomic "check quota, then insert" operation — see `try_insert_if_under_quota`.
/// Deliberately its own type rather than a `bool` or `Option`, so a call site can't accidentally
/// treat "quota exceeded" as success by forgetting to check a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    /// The row was inserted; the owner was under quota.
    Inserted {
        /// The directory slot (`0..limit`) the repository assigned this row — the smallest slot
        /// not already used by one of the owner's other active rows, computed from the same
        /// locked read as the quota count so two concurrent inserts for the same owner can never
        /// be assigned the same slot. See [`crate::domain::cluster::Cluster::slot`].
        slot: u32,
    },
    /// The row was **not** inserted; the owner was already at or over `limit`.
    QuotaExceeded {
        /// How many active clusters the owner had at the time of the check.
        current_count: u32,
    },
}

#[cfg_attr(test, automock)]
#[async_trait]
pub trait ClusterRepository: Send + Sync {
    /// Atomically checks `owner`'s current cluster count against `limit`, and if under it, also
    /// assigns the row a free directory slot (see [`InsertOutcome::Inserted`]) and inserts it — all
    /// in one transaction. This is the fix for the check-then-insert race (two concurrent creates
    /// from the same owner must not both observe "under quota"), and — since it happens in the
    /// same transaction — also the fix for the equivalent race in slot assignment (two concurrent
    /// creates from the same owner must not be assigned the same slot). `cluster.slot`'s incoming
    /// value is ignored; the assigned slot in the returned [`InsertOutcome::Inserted`] is
    /// authoritative.
    ///
    /// # Arguments
    ///
    /// - `cluster`: the new cluster row to insert if the owner is under quota.
    /// - `limit`: the maximum number of active clusters `cluster.owner` may hold; also the number
    ///   of directory slots (`0..limit`) available to assign from.
    ///
    /// # Returns
    ///
    /// [`InsertOutcome::Inserted`] (carrying the assigned slot) if the row was inserted, or
    /// [`InsertOutcome::QuotaExceeded`] (with the owner's current count) if it wasn't.
    ///
    /// # Errors
    ///
    /// Returns a [`RepositoryError`] if the underlying storage operation fails.
    async fn try_insert_if_under_quota(
        &self,
        cluster: &Cluster,
        limit: u32,
    ) -> Result<InsertOutcome, RepositoryError>;

    /// Owner-scoped lookup. Deliberately the only way handlers look up a single cluster: a
    /// cluster that exists but isn't owned by the caller is indistinguishable from one that
    /// never existed, by construction, all the way up to the HTTP response.
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster to look up.
    /// - `owner`: the caller the cluster must belong to.
    ///
    /// # Returns
    ///
    /// `Some(cluster)` if a row with `id` exists and is owned by `owner`; `None` if it doesn't
    /// exist, or exists but belongs to someone else.
    ///
    /// # Errors
    ///
    /// Returns a [`RepositoryError`] if the underlying storage operation fails.
    async fn get_owned(
        &self,
        id: &ClusterId,
        owner: &ClientId,
    ) -> Result<Option<Cluster>, RepositoryError>;

    /// Unscoped lookup, for the TTL reaper and startup reconciliation only — both operate as the
    /// system, not on behalf of a specific caller.
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster to look up.
    ///
    /// # Returns
    ///
    /// `Some(cluster)` if a row with `id` exists, regardless of owner; `None` otherwise.
    ///
    /// # Errors
    ///
    /// Returns a [`RepositoryError`] if the underlying storage operation fails.
    async fn get_any(&self, id: &ClusterId) -> Result<Option<Cluster>, RepositoryError>;

    /// Every cluster owned by `owner`, regardless of state, for `GET /clusters`.
    ///
    /// # Arguments
    ///
    /// - `owner`: the caller whose clusters to list.
    ///
    /// # Returns
    ///
    /// Every persisted row owned by `owner`, in no particular order.
    ///
    /// # Errors
    ///
    /// Returns a [`RepositoryError`] if the underlying storage operation fails.
    async fn list_by_owner(&self, owner: &ClientId) -> Result<Vec<Cluster>, RepositoryError>;

    /// Every persisted row regardless of owner — reconciliation and the reaper scan this.
    ///
    /// # Returns
    ///
    /// Every persisted row, in no particular order.
    ///
    /// # Errors
    ///
    /// Returns a [`RepositoryError`] if the underlying storage operation fails.
    async fn list_all(&self) -> Result<Vec<Cluster>, RepositoryError>;

    /// Overwrites a cluster row's persisted lifecycle state.
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster to update.
    /// - `state`: the new state to persist.
    ///
    /// # Returns
    ///
    /// Nothing, on success.
    ///
    /// # Errors
    ///
    /// Returns a [`RepositoryError`] if the underlying storage operation fails.
    async fn update_state(
        &self,
        id: &ClusterId,
        state: &ClusterState,
    ) -> Result<(), RepositoryError>;

    /// Records which worker was allocated to a cluster, so a restart mid-spawn can tell (during
    /// reconciliation) which pool slot to treat as still in use.
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster the worker was allocated to.
    /// - `worker`: the worker account that was allocated.
    ///
    /// # Returns
    ///
    /// Nothing, on success.
    ///
    /// # Errors
    ///
    /// Returns a [`RepositoryError`] if the underlying storage operation fails.
    async fn set_worker(&self, id: &ClusterId, worker: &WorkerUser) -> Result<(), RepositoryError>;

    /// The act that flips `GET /clusters/{id}` from `410 Gone` to `404 Not Found`.
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster row to delete.
    ///
    /// # Returns
    ///
    /// Nothing, on success.
    ///
    /// # Errors
    ///
    /// Returns a [`RepositoryError`] if the underlying storage operation fails.
    async fn delete(&self, id: &ClusterId) -> Result<(), RepositoryError>;
}
