//! Runs once at startup, before the HTTP listener binds, so `list`/`info` never contradict real
//! backend state right after a restart:
//!
//! - `Spawning` rows have no process still working on them (the one that was got killed) — no
//!   way to know how far it got, so they're force-transitioned to `Deleting { AdminForced }` and
//!   torn down immediately (idempotent either way).
//! - `Ready` rows whose backend reports the resource is gone become `Failed { "lost after
//!   restart" }` rather than trying to resume them.
//! - `Deleting` rows are always re-torn-down (idempotent, resumes cleanly whatever step a prior
//!   process got interrupted at).
//! - `Failed` rows are left alone — by design they keep holding their directory until the normal
//!   reaper grace period reaps them, restart or not.

use crate::domain::cluster::{ClusterState, DeleteReason};
use crate::service::deps::TaskDeps;
use crate::service::teardown_task;

/// Runs one reconciliation pass over every persisted cluster row, comparing it against real
/// backend state and forcing it into a consistent state after a possible crash/restart — see the
/// per-state policy in the module-level docs above.
///
/// # Arguments
///
/// - `deps`: shared dependencies (repository, backends, clock, etc.) used to read and update
///   cluster rows and query backend liveness.
///
/// # Panics
///
/// Never — failures are logged and the affected row is left for a later pass rather than
/// aborting the whole reconciliation sweep.
pub async fn run(deps: &TaskDeps) {
    let rows = match deps.repository.list_all().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(error = %error, "reconciliation failed to list clusters");
            return;
        }
    };

    for cluster in rows {
        match &cluster.state {
            ClusterState::Spawning { .. } => {
                tracing::warn!(cluster_id = %cluster.id, "found Spawning cluster at startup with no owning process; forcing teardown");
                let deleting_state = ClusterState::Deleting {
                    deleting_since: deps.clock.now(),
                    reason: DeleteReason::AdminForced,
                };
                if let Err(error) = deps
                    .repository
                    .update_state(&cluster.id, &deleting_state)
                    .await
                {
                    tracing::error!(cluster_id = %cluster.id, error = %error, "failed to mark stuck Spawning cluster as Deleting");
                    continue;
                }
                let deleting_cluster = crate::domain::cluster::Cluster {
                    state: deleting_state,
                    ..cluster
                };
                teardown_task::teardown(deps, &deleting_cluster).await;
            }
            ClusterState::Ready { .. } => {
                let Some(backend) = deps.backends.get(&cluster.service.kind) else {
                    continue;
                };
                match backend.is_alive(&cluster.id).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(cluster_id = %cluster.id, "Ready cluster's backend resource is gone after restart");
                        let failed_state = ClusterState::Failed {
                            failed_at: deps.clock.now(),
                            error_summary: "lost after restart".to_string(),
                        };
                        if let Err(error) = deps
                            .repository
                            .update_state(&cluster.id, &failed_state)
                            .await
                        {
                            tracing::error!(cluster_id = %cluster.id, error = %error, "failed to mark lost Ready cluster as Failed");
                        }
                    }
                    Err(error) => {
                        tracing::error!(cluster_id = %cluster.id, error = %error, "failed to check liveness of Ready cluster");
                    }
                }
            }
            ClusterState::Deleting { .. } => {
                teardown_task::teardown(deps, &cluster).await;
            }
            ClusterState::Failed { .. } => {
                // Left as-is: still holds its directory until the reaper's grace period passes.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::backends::ClusterBackend;
    use crate::client_workers::ClientWorkers;
    use crate::domain::cluster::{Cluster, ClusterError, ClusterState, DeleteReason};
    use crate::domain::ids::{ClientId, ClusterId, WorkerUser};
    use crate::domain::service_kind::{ConnectionInfo, ServiceKind, ServiceSpec};
    use crate::ports::clock::FakeClock;
    use crate::ports::repository::{ClusterRepository, InsertOutcome, RepositoryError};
    use crate::redacted::Redacted;
    use crate::service::deps::TaskDeps;
    use crate::test_support::{InMemoryClusterRepository, NoopPrivilegedExecutor};
    use async_trait::async_trait;
    use chrono::{TimeDelta, Utc};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct ScriptedAliveness {
        alive: AtomicBool,
    }

    #[async_trait]
    impl ClusterBackend for ScriptedAliveness {
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
            unreachable!("reconciliation tests never call spawn")
        }

        async fn teardown(&self, _cluster_id: &ClusterId) -> Result<(), ClusterError> {
            Ok(())
        }

        async fn is_alive(&self, _cluster_id: &ClusterId) -> Result<bool, ClusterError> {
            Ok(self.alive.load(Ordering::SeqCst))
        }
    }

    fn base_cluster(state: ClusterState, worker: Option<WorkerUser>) -> Cluster {
        Cluster {
            id: ClusterId::new(ulid::Ulid::r#gen()),
            owner: ClientId::new("agent"),
            service: ServiceSpec {
                kind: ServiceKind::Postgres,
                pgvector: false,
            },
            requested_ttl: TimeDelta::seconds(60),
            requested_at: Utc::now(),
            state,
            worker,
            slot: 0,
        }
    }

    fn deps(backend_alive: bool) -> (Arc<TaskDeps>, Arc<InMemoryClusterRepository>) {
        let repository = Arc::new(InMemoryClusterRepository::new());
        let deps = Arc::new(TaskDeps {
            repository: repository.clone(),
            client_workers: Arc::new(ClientWorkers::new(HashMap::new())),
            privileged_exec: Arc::new(NoopPrivilegedExecutor),
            backends: HashMap::from([(
                ServiceKind::Postgres,
                Arc::new(ScriptedAliveness {
                    alive: AtomicBool::new(backend_alive),
                }) as Arc<dyn ClusterBackend>,
            )]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
        });
        (deps, repository)
    }

    #[tokio::test]
    async fn stuck_spawning_cluster_is_forced_to_deleting_and_torn_down() {
        let worker = WorkerUser::new("openbrain-agent", 2000, 2000);
        let (deps, repository) = deps(true);
        let cluster = base_cluster(
            ClusterState::Spawning {
                started_at: Utc::now(),
            },
            Some(worker),
        );
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        run(&deps).await;

        assert!(
            repository
                .get_any(&cluster.id)
                .await
                .expect("query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn ready_cluster_with_live_backend_is_left_alone() {
        let (deps, repository) = deps(true);
        let connection = ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: 5432,
            dbname: "app_salmon".to_string(),
            user: "app_salmon".to_string(),
            password: Redacted::new("hunter2".to_string()),
        };
        let cluster = base_cluster(
            ClusterState::Ready {
                ready_at: Utc::now(),
                decommission_at: Utc::now(),
                connection,
            },
            None,
        );
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        run(&deps).await;

        let stored = repository
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        assert!(matches!(stored.state, ClusterState::Ready { .. }));
    }

    #[tokio::test]
    async fn ready_cluster_with_dead_backend_becomes_failed() {
        let (deps, repository) = deps(false);
        let connection = ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: 5432,
            dbname: "app_salmon".to_string(),
            user: "app_salmon".to_string(),
            password: Redacted::new("hunter2".to_string()),
        };
        let cluster = base_cluster(
            ClusterState::Ready {
                ready_at: Utc::now(),
                decommission_at: Utc::now(),
                connection,
            },
            None,
        );
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        run(&deps).await;

        let stored = repository
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        match stored.state {
            ClusterState::Failed { error_summary, .. } => {
                assert_eq!(error_summary, "lost after restart");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deleting_cluster_is_always_re_torn_down() {
        let worker = WorkerUser::new("openbrain-agent", 2000, 2000);
        let (deps, repository) = deps(true);
        let cluster = base_cluster(
            ClusterState::Deleting {
                deleting_since: Utc::now(),
                reason: DeleteReason::UserRequested,
            },
            Some(worker),
        );
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        run(&deps).await;

        assert!(
            repository
                .get_any(&cluster.id)
                .await
                .expect("query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn failed_cluster_is_left_alone() {
        let worker = WorkerUser::new("openbrain-agent", 2000, 2000);
        let (deps, repository) = deps(true);
        let cluster = base_cluster(
            ClusterState::Failed {
                failed_at: Utc::now(),
                error_summary: "boom".to_string(),
            },
            Some(worker),
        );
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        run(&deps).await;

        let stored = repository
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row still present");
        assert!(matches!(stored.state, ClusterState::Failed { .. }));
    }

    /// Wraps `InMemoryClusterRepository`, letting tests inject a failure from specific methods —
    /// for exercising reconciliation's "log and continue" error-handling branches.
    struct FlakyRepository {
        inner: InMemoryClusterRepository,
        fail_list_all: AtomicBool,
        fail_update_state: AtomicBool,
    }

    fn simulated_failure() -> RepositoryError {
        RepositoryError::Migration("simulated failure".to_string())
    }

    #[async_trait]
    impl ClusterRepository for FlakyRepository {
        async fn try_insert_if_under_quota(
            &self,
            cluster: &Cluster,
            limit: u32,
        ) -> Result<InsertOutcome, RepositoryError> {
            self.inner.try_insert_if_under_quota(cluster, limit).await
        }

        async fn get_owned(
            &self,
            id: &ClusterId,
            owner: &ClientId,
        ) -> Result<Option<Cluster>, RepositoryError> {
            self.inner.get_owned(id, owner).await
        }

        async fn get_any(&self, id: &ClusterId) -> Result<Option<Cluster>, RepositoryError> {
            self.inner.get_any(id).await
        }

        async fn list_by_owner(&self, owner: &ClientId) -> Result<Vec<Cluster>, RepositoryError> {
            self.inner.list_by_owner(owner).await
        }

        async fn list_all(&self) -> Result<Vec<Cluster>, RepositoryError> {
            if self.fail_list_all.load(Ordering::SeqCst) {
                return Err(simulated_failure());
            }
            self.inner.list_all().await
        }

        async fn update_state(
            &self,
            id: &ClusterId,
            state: &ClusterState,
        ) -> Result<(), RepositoryError> {
            if self.fail_update_state.load(Ordering::SeqCst) {
                return Err(simulated_failure());
            }
            self.inner.update_state(id, state).await
        }

        async fn set_worker(
            &self,
            id: &ClusterId,
            worker: &WorkerUser,
        ) -> Result<(), RepositoryError> {
            self.inner.set_worker(id, worker).await
        }

        async fn delete(&self, id: &ClusterId) -> Result<(), RepositoryError> {
            self.inner.delete(id).await
        }
    }

    #[tokio::test]
    async fn list_all_failure_is_logged_and_the_sweep_is_skipped() {
        let repository = Arc::new(FlakyRepository {
            inner: InMemoryClusterRepository::new(),
            fail_list_all: AtomicBool::new(true),
            fail_update_state: AtomicBool::new(false),
        });
        let deps = TaskDeps {
            repository,
            client_workers: Arc::new(ClientWorkers::new(HashMap::new())),
            privileged_exec: Arc::new(NoopPrivilegedExecutor),
            backends: HashMap::new(),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
        };

        // Must not panic even though list_all fails.
        run(&deps).await;
    }

    #[tokio::test]
    async fn update_state_failure_while_forcing_stuck_spawning_is_logged_and_skipped() {
        let repository = Arc::new(FlakyRepository {
            inner: InMemoryClusterRepository::new(),
            fail_list_all: AtomicBool::new(false),
            fail_update_state: AtomicBool::new(true),
        });
        let worker = WorkerUser::new("openbrain-agent", 2000, 2000);
        let cluster = base_cluster(
            ClusterState::Spawning {
                started_at: Utc::now(),
            },
            Some(worker),
        );
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        let deps = TaskDeps {
            repository: repository.clone(),
            client_workers: Arc::new(ClientWorkers::new(HashMap::new())),
            privileged_exec: Arc::new(NoopPrivilegedExecutor),
            backends: HashMap::new(),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
        };

        // Doesn't panic; the row is left as-is (still Spawning) since the update failed.
        run(&deps).await;
        let stored = repository
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        assert!(matches!(stored.state, ClusterState::Spawning { .. }));
    }

    #[tokio::test]
    async fn update_state_failure_while_marking_ready_as_failed_is_logged() {
        let repository = Arc::new(FlakyRepository {
            inner: InMemoryClusterRepository::new(),
            fail_list_all: AtomicBool::new(false),
            fail_update_state: AtomicBool::new(true),
        });
        let connection = ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: 5432,
            dbname: "app_salmon".to_string(),
            user: "app_salmon".to_string(),
            password: Redacted::new("hunter2".to_string()),
        };
        let cluster = base_cluster(
            ClusterState::Ready {
                ready_at: Utc::now(),
                decommission_at: Utc::now(),
                connection,
            },
            None,
        );
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        let deps = TaskDeps {
            repository: repository.clone(),
            client_workers: Arc::new(ClientWorkers::new(HashMap::new())),
            privileged_exec: Arc::new(NoopPrivilegedExecutor),
            backends: HashMap::from([(
                ServiceKind::Postgres,
                Arc::new(ScriptedAliveness {
                    alive: AtomicBool::new(false),
                }) as Arc<dyn ClusterBackend>,
            )]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
        };

        run(&deps).await;
        let stored = repository
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        assert!(
            matches!(stored.state, ClusterState::Ready { .. }),
            "state unchanged since the update failed"
        );
    }

    struct AlwaysErrorsAliveness;

    #[async_trait]
    impl ClusterBackend for AlwaysErrorsAliveness {
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
            unreachable!("reconciliation tests never call spawn")
        }

        async fn teardown(&self, _cluster_id: &ClusterId) -> Result<(), ClusterError> {
            Ok(())
        }

        async fn is_alive(&self, _cluster_id: &ClusterId) -> Result<bool, ClusterError> {
            Err(ClusterError::BackendSpawnFailed(
                "simulated is_alive failure".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn is_alive_error_is_logged_and_cluster_left_alone() {
        let repository = Arc::new(InMemoryClusterRepository::new());
        let connection = ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: 5432,
            dbname: "app_salmon".to_string(),
            user: "app_salmon".to_string(),
            password: Redacted::new("hunter2".to_string()),
        };
        let cluster = base_cluster(
            ClusterState::Ready {
                ready_at: Utc::now(),
                decommission_at: Utc::now(),
                connection,
            },
            None,
        );
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        let deps = TaskDeps {
            repository: repository.clone(),
            client_workers: Arc::new(ClientWorkers::new(HashMap::new())),
            privileged_exec: Arc::new(NoopPrivilegedExecutor),
            backends: HashMap::from([(
                ServiceKind::Postgres,
                Arc::new(AlwaysErrorsAliveness) as Arc<dyn ClusterBackend>,
            )]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
        };

        run(&deps).await;
        let stored = repository
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        assert!(matches!(stored.state, ClusterState::Ready { .. }));
    }

    #[tokio::test]
    async fn ready_cluster_with_no_registered_backend_is_left_alone() {
        let repository = Arc::new(InMemoryClusterRepository::new());
        let connection = ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: 5432,
            dbname: "app_salmon".to_string(),
            user: "app_salmon".to_string(),
            password: Redacted::new("hunter2".to_string()),
        };
        let cluster = base_cluster(
            ClusterState::Ready {
                ready_at: Utc::now(),
                decommission_at: Utc::now(),
                connection,
            },
            None,
        );
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        let deps = TaskDeps {
            repository: repository.clone(),
            client_workers: Arc::new(ClientWorkers::new(HashMap::new())),
            privileged_exec: Arc::new(NoopPrivilegedExecutor),
            backends: HashMap::new(),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
        };

        run(&deps).await;
        let stored = repository
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        assert!(matches!(stored.state, ClusterState::Ready { .. }));
    }
}
