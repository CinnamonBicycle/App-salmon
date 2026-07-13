//! Tears down whatever a cluster's backend + worker directory hold, then removes its row —
//! that last step is what flips `GET /clusters/{id}` from `410 Gone` to `404 Not Found`.
//!
//! `teardown` (the function, not the task) is deliberately reusable: `run` is the normal path
//! (a `Deleting` cluster that was already `Ready`/`Failed`), and `spawn_task` calls the same
//! function directly when a spawn is cancelled mid-flight — that's what guarantees only one task
//! is ever tearing a given cluster down, rather than a fresh `teardown_task` racing against a
//! spawn task that's cleaning up after itself.
//!
//! Each step logs and continues on failure rather than aborting, so a single failed step (e.g.
//! the daemon being briefly unreachable) doesn't leave the row stuck in `Deleting` forever and
//! permanently consuming a quota slot and a worker.

use crate::client_workers::worker_data_dir;
use crate::domain::cluster::Cluster;
use crate::ports::privileged_exec::PrivilegedCommand;
use crate::service::deps::TaskDeps;

/// Tears down whatever `cluster`'s backend and worker directory hold, then deletes its row.
/// Idempotent and best-effort at each step: a failure at any point is logged and cleanup
/// continues with the next step (rather than aborting), so a transient failure (e.g. the daemon
/// briefly unreachable) doesn't leave the row stuck in `Deleting` forever, permanently consuming a
/// quota slot and a worker. Deleting the row last is what flips `GET /clusters/{id}` from `410
/// Gone` to `404 Not Found`.
///
/// # Arguments
///
/// - `deps`: shared task dependencies — the registered backend for `cluster`'s service kind (if
///   any), the privileged executor used to wipe the worker directory, and the repository.
/// - `cluster`: the cluster to tear down. If `cluster.worker` is `None` (a spawn that never got
///   as far as resolving one), the worker-directory-wipe step is skipped.
pub async fn teardown(deps: &TaskDeps, cluster: &Cluster) {
    if let Some(backend) = deps.backends.get(&cluster.service.kind)
        && let Err(error) = backend.teardown(&cluster.id).await
    {
        tracing::warn!(cluster_id = %cluster.id, error = %error, "backend teardown failed; continuing cleanup");
    }

    if let Some(worker) = &cluster.worker {
        let path = worker_data_dir(&deps.worker_data_dir_base, worker, cluster.slot);
        let wipe = deps
            .privileged_exec
            .run_as(
                worker,
                PrivilegedCommand::WipeWorkerDir {
                    path: path.display().to_string(),
                },
            )
            .await;
        if let Err(error) = wipe {
            tracing::warn!(cluster_id = %cluster.id, worker = %worker, error = %error, "wipe worker dir failed; continuing cleanup");
        }
    }

    if let Err(error) = deps.repository.delete(&cluster.id).await {
        tracing::error!(cluster_id = %cluster.id, error = %error, "failed to delete cluster row after teardown");
    }
}

/// Entry point for the normal (non-cancellation) teardown path: a `Deleting` cluster that was
/// already `Ready`/`Failed`. Thin wrapper around [`teardown`] so it can be handed directly to
/// `tokio::spawn`; `spawn_task` calls [`teardown`] itself instead of this, since it already holds
/// a `&Cluster` rather than an owned one.
///
/// # Arguments
///
/// - `deps`: shared task dependencies, passed through to [`teardown`].
/// - `cluster`: the cluster to tear down.
pub async fn run(deps: std::sync::Arc<TaskDeps>, cluster: Cluster) {
    teardown(&deps, &cluster).await;
}

#[cfg(test)]
mod tests {
    use super::teardown;
    use crate::backends::ClusterBackend;
    use crate::client_workers::ClientWorkers;
    use crate::domain::cluster::{Cluster, ClusterState, DeleteReason};
    use crate::domain::ids::{ClientId, ClusterId, WorkerUser};
    use crate::domain::service_kind::{ConnectionInfo, ServiceKind, ServiceSpec};
    use crate::ports::clock::FakeClock;
    use crate::ports::privileged_exec::{
        CommandOutput, PrivilegedCommand, PrivilegedExecError, PrivilegedExecutor,
    };
    use crate::ports::repository::ClusterRepository;
    use crate::service::deps::TaskDeps;
    use crate::test_support::InMemoryClusterRepository;
    use async_trait::async_trait;
    use chrono::{TimeDelta, Utc};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct RecordingBackend {
        kind: ServiceKind,
        teardown_calls: AtomicUsize,
        fail_teardown: AtomicBool,
    }

    #[async_trait]
    impl ClusterBackend for RecordingBackend {
        fn kind(&self) -> ServiceKind {
            self.kind
        }

        async fn spawn(
            &self,
            _cluster_id: &ClusterId,
            _worker: &WorkerUser,
            _slot: u32,
            _service: &ServiceSpec,
        ) -> Result<ConnectionInfo, crate::domain::cluster::ClusterError> {
            unreachable!("teardown tests never call spawn")
        }

        async fn teardown(
            &self,
            _cluster_id: &ClusterId,
        ) -> Result<(), crate::domain::cluster::ClusterError> {
            self.teardown_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_teardown.load(Ordering::SeqCst) {
                return Err(crate::domain::cluster::ClusterError::BackendSpawnFailed(
                    "simulated teardown failure".to_string(),
                ));
            }
            Ok(())
        }

        async fn is_alive(
            &self,
            _cluster_id: &ClusterId,
        ) -> Result<bool, crate::domain::cluster::ClusterError> {
            unreachable!("teardown tests never call is_alive")
        }
    }

    struct RecordingExecutor {
        wipe_calls: Mutex<Vec<String>>,
        fail_wipe: AtomicBool,
    }

    #[async_trait]
    impl PrivilegedExecutor for RecordingExecutor {
        async fn run_as(
            &self,
            worker: &WorkerUser,
            command: PrivilegedCommand,
        ) -> Result<CommandOutput, PrivilegedExecError> {
            if let PrivilegedCommand::WipeWorkerDir { path } = command {
                self.wipe_calls.lock().expect("lock").push(path);
                if self.fail_wipe.load(Ordering::SeqCst) {
                    return Err(PrivilegedExecError::NonZeroExit {
                        worker: worker.clone(),
                        status: 1,
                        stderr: "simulated wipe failure".to_string(),
                    });
                }
            }
            Ok(CommandOutput {
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    fn deleting_cluster(worker: Option<WorkerUser>) -> Cluster {
        Cluster {
            id: ClusterId::new(ulid::Ulid::r#gen()),
            owner: ClientId::new("agent"),
            service: ServiceSpec {
                kind: ServiceKind::Postgres,
                pgvector: false,
            },
            requested_ttl: TimeDelta::seconds(60),
            requested_at: Utc::now(),
            state: ClusterState::Deleting {
                deleting_since: Utc::now(),
                reason: DeleteReason::UserRequested,
            },
            worker,
            slot: 0,
        }
    }

    #[tokio::test]
    async fn teardown_calls_backend_wipes_worker_directory_and_deletes_row() {
        let backend = Arc::new(RecordingBackend {
            kind: ServiceKind::Postgres,
            teardown_calls: AtomicUsize::new(0),
            fail_teardown: AtomicBool::new(false),
        });
        let executor = Arc::new(RecordingExecutor {
            wipe_calls: Mutex::new(Vec::new()),
            fail_wipe: AtomicBool::new(false),
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let repository = Arc::new(InMemoryClusterRepository::new());

        let cluster = deleting_cluster(Some(worker.clone()));
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        let deps = TaskDeps {
            repository: repository.clone(),
            client_workers: Arc::new(ClientWorkers::new(HashMap::new())),
            privileged_exec: executor.clone(),
            backends: HashMap::from([(
                ServiceKind::Postgres,
                backend.clone() as Arc<dyn ClusterBackend>,
            )]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            tar_staging_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/tar-staging"),
            tar_limits: crate::domain::tar_validation::TarLimits {
                max_entry_bytes: 10_485_760,
                max_total_bytes: 52_428_800,
            },
        };

        teardown(&deps, &cluster).await;

        assert_eq!(backend.teardown_calls.load(Ordering::SeqCst), 1);
        assert_eq!(executor.wipe_calls.lock().expect("lock").len(), 1);
        assert!(
            repository
                .get_any(&cluster.id)
                .await
                .expect("query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn teardown_without_a_worker_skips_wipe_and_release_but_still_deletes() {
        let backend = Arc::new(RecordingBackend {
            kind: ServiceKind::Postgres,
            teardown_calls: AtomicUsize::new(0),
            fail_teardown: AtomicBool::new(false),
        });
        let executor = Arc::new(RecordingExecutor {
            wipe_calls: Mutex::new(Vec::new()),
            fail_wipe: AtomicBool::new(false),
        });
        let repository = Arc::new(InMemoryClusterRepository::new());
        let cluster = deleting_cluster(None);
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        let deps = TaskDeps {
            repository: repository.clone(),
            client_workers: Arc::new(ClientWorkers::new(HashMap::new())),
            privileged_exec: executor.clone(),
            backends: HashMap::from([(
                ServiceKind::Postgres,
                backend.clone() as Arc<dyn ClusterBackend>,
            )]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            tar_staging_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/tar-staging"),
            tar_limits: crate::domain::tar_validation::TarLimits {
                max_entry_bytes: 10_485_760,
                max_total_bytes: 52_428_800,
            },
        };

        teardown(&deps, &cluster).await;

        assert!(executor.wipe_calls.lock().expect("lock").is_empty());
        assert!(
            repository
                .get_any(&cluster.id)
                .await
                .expect("query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn teardown_deletes_the_row_even_if_backend_teardown_fails() {
        let backend = Arc::new(RecordingBackend {
            kind: ServiceKind::Postgres,
            teardown_calls: AtomicUsize::new(0),
            fail_teardown: AtomicBool::new(true),
        });
        let executor = Arc::new(RecordingExecutor {
            wipe_calls: Mutex::new(Vec::new()),
            fail_wipe: AtomicBool::new(false),
        });
        let repository = Arc::new(InMemoryClusterRepository::new());
        let cluster = deleting_cluster(None);
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        let deps = TaskDeps {
            repository: repository.clone(),
            client_workers: Arc::new(ClientWorkers::new(HashMap::new())),
            privileged_exec: executor,
            backends: HashMap::from([(ServiceKind::Postgres, backend as Arc<dyn ClusterBackend>)]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            tar_staging_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/tar-staging"),
            tar_limits: crate::domain::tar_validation::TarLimits {
                max_entry_bytes: 10_485_760,
                max_total_bytes: 52_428_800,
            },
        };

        teardown(&deps, &cluster).await;

        assert!(
            repository
                .get_any(&cluster.id)
                .await
                .expect("query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn teardown_skips_backends_with_no_registered_kind() {
        // Not realistic in phase 1 (only Postgres exists) but guards the lookup being a no-op
        // rather than a panic if a future service kind's backend isn't registered.
        let repository = Arc::new(InMemoryClusterRepository::new());
        let cluster = deleting_cluster(None);
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        let deps = TaskDeps {
            repository: repository.clone(),
            client_workers: Arc::new(ClientWorkers::new(HashMap::new())),
            privileged_exec: Arc::new(RecordingExecutor {
                wipe_calls: Mutex::new(Vec::new()),
                fail_wipe: AtomicBool::new(false),
            }),
            backends: HashMap::new(),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            tar_staging_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/tar-staging"),
            tar_limits: crate::domain::tar_validation::TarLimits {
                max_entry_bytes: 10_485_760,
                max_total_bytes: 52_428_800,
            },
        };

        teardown(&deps, &cluster).await;

        assert!(
            repository
                .get_any(&cluster.id)
                .await
                .expect("query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn wipe_failure_is_logged_but_teardown_still_deletes_the_row() {
        let backend = Arc::new(RecordingBackend {
            kind: ServiceKind::Postgres,
            teardown_calls: AtomicUsize::new(0),
            fail_teardown: AtomicBool::new(false),
        });
        let executor = Arc::new(RecordingExecutor {
            wipe_calls: Mutex::new(Vec::new()),
            fail_wipe: AtomicBool::new(true),
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let repository = Arc::new(InMemoryClusterRepository::new());

        let cluster = deleting_cluster(Some(worker.clone()));
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        let deps = TaskDeps {
            repository: repository.clone(),
            client_workers: Arc::new(ClientWorkers::new(HashMap::new())),
            privileged_exec: executor.clone(),
            backends: HashMap::from([(
                ServiceKind::Postgres,
                backend.clone() as Arc<dyn ClusterBackend>,
            )]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            tar_staging_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/tar-staging"),
            tar_limits: crate::domain::tar_validation::TarLimits {
                max_entry_bytes: 10_485_760,
                max_total_bytes: 52_428_800,
            },
        };

        teardown(&deps, &cluster).await;

        assert_eq!(executor.wipe_calls.lock().expect("lock").len(), 1);
        assert!(
            repository
                .get_any(&cluster.id)
                .await
                .expect("query")
                .is_none(),
            "row is still deleted even though wiping the worker directory failed"
        );
    }

    #[tokio::test]
    async fn repository_delete_failure_is_logged_not_fatal() {
        let backend = Arc::new(RecordingBackend {
            kind: ServiceKind::Postgres,
            teardown_calls: AtomicUsize::new(0),
            fail_teardown: AtomicBool::new(false),
        });
        let cluster = deleting_cluster(None);

        // `teardown` with no worker attached calls exactly one repository method: `delete`. A
        // mock lets this test say precisely that, instead of hand-rolling a repository that
        // delegates six unrelated methods to a real fake purely to satisfy the trait.
        let mut repository = crate::ports::repository::MockClusterRepository::new();
        repository.expect_delete().times(1).returning(|_| {
            Err(crate::ports::repository::RepositoryError::Migration(
                "simulated delete failure".to_string(),
            ))
        });

        let deps = TaskDeps {
            repository: Arc::new(repository),
            client_workers: Arc::new(ClientWorkers::new(HashMap::new())),
            privileged_exec: Arc::new(RecordingExecutor {
                wipe_calls: Mutex::new(Vec::new()),
                fail_wipe: AtomicBool::new(false),
            }),
            backends: HashMap::from([(ServiceKind::Postgres, backend as Arc<dyn ClusterBackend>)]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            tar_staging_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/tar-staging"),
            tar_limits: crate::domain::tar_validation::TarLimits {
                max_entry_bytes: 10_485_760,
                max_total_bytes: 52_428_800,
            },
        };

        // Must not panic even though delete() fails; the mock's own `.times(1)` expectation
        // (checked on drop) is the assertion that `delete` was actually reached.
        teardown(&deps, &cluster).await;
    }
}
