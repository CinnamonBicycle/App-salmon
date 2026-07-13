//! Test doubles shared across service/http unit tests. Compiled only under `cfg(test)`.

#![cfg(test)]

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use ulid::Ulid;

use crate::backends::ClusterBackend;
use crate::domain::cluster::{Cluster, ClusterError, ClusterState};
use crate::domain::ids::{ClientId, ClusterId, WorkerUser};
use crate::domain::service_kind::{
    ConnectionInfo, PostgresConnectionInfo, ServiceKind, ServiceSpec,
};
use crate::ports::privileged_exec::{
    CommandOutput, PrivilegedCommand, PrivilegedExecError, PrivilegedExecutor,
};
use crate::ports::repository::{ClusterRepository, InsertOutcome, RepositoryError};
use crate::ports::secrets::SecretGenerator;

#[derive(Default)]
pub struct InMemoryClusterRepository {
    rows: Mutex<HashMap<ClusterId, Cluster>>,
}

impl InMemoryClusterRepository {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ClusterRepository for InMemoryClusterRepository {
    async fn try_insert_if_under_quota(
        &self,
        cluster: &Cluster,
        limit: u32,
    ) -> Result<InsertOutcome, RepositoryError> {
        let mut rows = self.rows.lock().expect("lock");
        let owners_rows: Vec<&Cluster> = rows
            .values()
            .filter(|row| row.owner == cluster.owner)
            .collect();
        let current_count = u32::try_from(owners_rows.len()).unwrap_or(u32::MAX);
        if current_count >= limit {
            return Ok(InsertOutcome::QuotaExceeded { current_count });
        }
        let used_slots: std::collections::HashSet<u32> =
            owners_rows.iter().map(|row| row.slot).collect();
        let slot = (0..limit)
            .find(|candidate| !used_slots.contains(candidate))
            .expect("a free slot exists whenever current_count < limit");
        let mut to_store = cluster.clone();
        to_store.slot = slot;
        rows.insert(cluster.id, to_store);
        Ok(InsertOutcome::Inserted { slot })
    }

    async fn get_owned(
        &self,
        id: &ClusterId,
        owner: &ClientId,
    ) -> Result<Option<Cluster>, RepositoryError> {
        let rows = self.rows.lock().expect("lock");
        Ok(rows.get(id).filter(|row| &row.owner == owner).cloned())
    }

    async fn get_any(&self, id: &ClusterId) -> Result<Option<Cluster>, RepositoryError> {
        let rows = self.rows.lock().expect("lock");
        Ok(rows.get(id).cloned())
    }

    async fn list_by_owner(&self, owner: &ClientId) -> Result<Vec<Cluster>, RepositoryError> {
        let rows = self.rows.lock().expect("lock");
        Ok(rows
            .values()
            .filter(|row| &row.owner == owner)
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<Cluster>, RepositoryError> {
        let rows = self.rows.lock().expect("lock");
        Ok(rows.values().cloned().collect())
    }

    async fn update_state(
        &self,
        id: &ClusterId,
        state: &ClusterState,
    ) -> Result<(), RepositoryError> {
        let mut rows = self.rows.lock().expect("lock");
        if let Some(row) = rows.get_mut(id) {
            row.state = state.clone();
        }
        Ok(())
    }

    async fn set_worker(
        &self,
        id: &ClusterId,
        worker: &crate::domain::ids::WorkerUser,
    ) -> Result<(), RepositoryError> {
        let mut rows = self.rows.lock().expect("lock");
        if let Some(row) = rows.get_mut(id) {
            row.worker = Some(worker.clone());
        }
        Ok(())
    }

    async fn delete(&self, id: &ClusterId) -> Result<(), RepositoryError> {
        let mut rows = self.rows.lock().expect("lock");
        rows.remove(id);
        Ok(())
    }
}

/// Deterministic, reproducible IDs/passwords for assertions — never real randomness.
#[derive(Default)]
pub struct FakeSecretGenerator {
    counter: AtomicU64,
}

impl SecretGenerator for FakeSecretGenerator {
    fn cluster_id(&self) -> ClusterId {
        let count = u128::from(self.counter.fetch_add(1, Ordering::SeqCst));
        ClusterId::new(Ulid::from_parts(0, count))
    }

    fn db_password(&self, len: usize) -> String {
        "p".repeat(len)
    }
}

/// A `PrivilegedExecutor` that always succeeds and records nothing — for tests where privileged
/// exec is a required dependency but not what's under test.
#[derive(Default)]
pub struct NoopPrivilegedExecutor;

#[async_trait]
impl PrivilegedExecutor for NoopPrivilegedExecutor {
    async fn run_as(
        &self,
        _worker: &WorkerUser,
        _command: PrivilegedCommand,
    ) -> Result<CommandOutput, PrivilegedExecError> {
        Ok(CommandOutput {
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// A `ClusterBackend` whose `spawn` never resolves. Used by HTTP-layer tests: the handler kicks
/// off a real background `spawn_task` on a real `tokio::spawn`, and this keeps that task
/// permanently stuck *before* it can write any state — so a test that then sets a specific state
/// directly via the repository (to exercise `GET` in that state) isn't racing against the
/// background task overwriting it.
#[derive(Default)]
pub struct HangingClusterBackend;

#[async_trait]
impl ClusterBackend for HangingClusterBackend {
    fn kind(&self) -> ServiceKind {
        ServiceKind::Postgres
    }

    async fn spawn(
        &self,
        _cluster_id: &ClusterId,
        _worker: &WorkerUser,
        _slot: u32,
        _service: &ServiceSpec,
    ) -> Result<ConnectionInfo, ClusterError> {
        std::future::pending().await
    }

    async fn teardown(&self, _cluster_id: &ClusterId) -> Result<(), ClusterError> {
        Ok(())
    }

    async fn is_alive(&self, _cluster_id: &ClusterId) -> Result<bool, ClusterError> {
        Ok(true)
    }
}

/// A `ClusterBackend` whose `spawn` resolves immediately with canned connection info. Used where
/// a test needs the real background `spawn_task` (launched by the HTTP layer's `launch_spawn`)
/// to actually run to completion — e.g. to exercise the task-registry unregister step that only
/// happens once the task returns, which `HangingClusterBackend` deliberately never reaches.
#[derive(Default)]
pub struct FastSucceedingClusterBackend;

#[async_trait]
impl ClusterBackend for FastSucceedingClusterBackend {
    fn kind(&self) -> ServiceKind {
        ServiceKind::Postgres
    }

    async fn spawn(
        &self,
        _cluster_id: &ClusterId,
        _worker: &WorkerUser,
        _slot: u32,
        _service: &ServiceSpec,
    ) -> Result<ConnectionInfo, ClusterError> {
        Ok(ConnectionInfo::Postgres(PostgresConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: 55432,
            dbname: "app_salmon".to_string(),
            user: "app_salmon".to_string(),
            password: crate::redacted::Redacted::new("hunter2".to_string()),
        }))
    }

    async fn teardown(&self, _cluster_id: &ClusterId) -> Result<(), ClusterError> {
        Ok(())
    }

    async fn is_alive(&self, _cluster_id: &ClusterId) -> Result<bool, ClusterError> {
        Ok(true)
    }
}
