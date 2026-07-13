//! Runs a cluster's backend spawn in the background after `ClusterService::create` returns.
//! Persists the outcome (`Ready` or `Failed`) itself — this is a fire-and-forget background
//! task, not something with a caller waiting on a `Result`.
//!
//! Cancellation (a `DELETE` arriving while still `Spawning`) races against the spawn work via
//! `tokio::select!`. On cancellation, this task tears down whatever it had already allocated
//! itself, by calling the same [`teardown_task::teardown`] function `teardown_task::run` uses —
//! that's what guarantees exactly one task ever tears a given cluster down: the caller that
//! cancels a spawn does not *also* start a fresh teardown task racing against this one.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::client_workers::{ClientWorkerError, worker_data_dir};
use crate::domain::cluster::{Cluster, ClusterError, ClusterEvent, ClusterState, transition};
use crate::domain::service_kind::ConnectionInfo;
use crate::ports::container_runtime::DockerError;
use crate::ports::privileged_exec::PrivilegedCommand;
use crate::service::deps::TaskDeps;
use crate::service::teardown_task;

/// Maps an internal spawn failure to a coarse, user-facing summary safe to persist on the
/// cluster's `Failed` state and return from the API — never the raw error's `Display` text, which
/// could echo back request content (e.g. a Docker daemon error including the submitted container
/// spec, which carries the generated DB password).
///
/// # Arguments
///
/// - `error`: the internal error `do_spawn` (or a lower layer) produced.
///
/// # Returns
///
/// A short, sanitized summary string with no secret or internal-implementation detail in it.
fn sanitize(error: &ClusterError) -> String {
    match error {
        ClusterError::Docker(DockerError::HealthCheckTimeout { .. }) => {
            "container did not become healthy in time".to_string()
        }
        ClusterError::Docker(DockerError::ContainerNotHealthy { .. }) => {
            "container exited unexpectedly during startup".to_string()
        }
        ClusterError::Docker(_) => "container creation failed".to_string(),
        ClusterError::ClientWorker(_) => "worker preparation failed".to_string(),
        ClusterError::Repository(_) => "internal storage error".to_string(),
        // Already a coarse, backend-chosen summary — see `ClusterError::BackendSpawnFailed`.
        ClusterError::BackendSpawnFailed(message) => message.clone(),
        ClusterError::TtlOutOfBounds { .. }
        | ClusterError::QuotaExceeded { .. }
        | ClusterError::NotFound(_)
        | ClusterError::InvalidTransition { .. } => "spawn failed".to_string(),
    }
}

/// Resolves the owner's Unix account, prepares its per-cluster on-disk directory, and asks the
/// cluster's registered backend to actually spawn it. This is the fallible core of a spawn
/// attempt — `run` wraps this call in a `tokio::select!` against cancellation and handles
/// persisting the outcome.
///
/// # Arguments
///
/// - `deps`: shared task dependencies (repository, client-worker mapping, privileged executor,
///   registered backends, clock, worker data directory base).
/// - `cluster`: the cluster being spawned; mutated in place to record the resolved `worker` once
///   it's known, so the caller can use it for cleanup even if a later step fails.
///
/// # Returns
///
/// Connection details for the newly spawned backend resource, once it's up.
///
/// # Errors
///
/// Returns [`ClusterError::BackendSpawnFailed`] if no backend is registered for the cluster's
/// service kind, a [`crate::client_workers::ClientWorkerError`] (via `#[from]`) if the owner has
/// no configured account or the privileged directory-preparation command fails, or whatever error
/// the backend's own `spawn` call produces (including [`ClusterError::Repository`] if persisting
/// the resolved worker fails).
async fn do_spawn(deps: &TaskDeps, cluster: &mut Cluster) -> Result<ConnectionInfo, ClusterError> {
    let backend = deps
        .backends
        .get(&cluster.service.kind)
        .ok_or_else(|| {
            ClusterError::BackendSpawnFailed(
                "no backend registered for this service kind".to_string(),
            )
        })?
        .clone();

    let worker = deps.client_workers.get(&cluster.owner)?;
    cluster.worker = Some(worker.clone());
    deps.repository.set_worker(&cluster.id, &worker).await?;

    let path = worker_data_dir(&deps.worker_data_dir_base, &worker, cluster.slot);
    deps.privileged_exec
        .run_as(
            &worker,
            PrivilegedCommand::PrepareWorkerDir {
                path: path.display().to_string(),
            },
        )
        .await
        .map_err(ClientWorkerError::Prepare)?;

    backend
        .spawn(&cluster.id, &worker, cluster.slot, &cluster.service)
        .await
}

/// Drives one cluster's spawn attempt to completion in the background: races [`do_spawn`] against
/// `cancel`, then persists whatever the outcome was (`Ready`, `Failed`, or — on cancellation or a
/// concurrent delete racing ahead of `cancel` — tearing down instead of persisting an outcome at
/// all; see the module docs above for why that re-check exists).
///
/// # Arguments
///
/// - `deps`: shared task dependencies, passed through to [`do_spawn`] and
///   [`teardown_task::teardown`].
/// - `cluster`: the cluster to spawn, as it existed when this task was launched (still
///   `Spawning`); mutated locally as the spawn progresses (e.g. once a worker is acquired), but
///   the row's *persisted* state is only ever read fresh via `deps.repository`, never assumed
///   from this local copy, before writing a final outcome.
/// - `cancel`: signaled by the HTTP layer if a `DELETE` arrives for this cluster while it's still
///   registered as in-flight.
pub async fn run(deps: Arc<TaskDeps>, mut cluster: Cluster, cancel: CancellationToken) {
    let outcome = tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        result = do_spawn(&deps, &mut cluster) => Some(result),
    };

    let Some(spawn_result) = outcome else {
        tracing::info!(cluster_id = %cluster.id, "spawn cancelled; tearing down partial state");
        teardown_task::teardown(&deps, &cluster).await;
        return;
    };

    // `do_spawn` ran to completion (either outcome). A concurrent `DELETE` can race ahead of us
    // between `do_spawn` finishing and this point — `request_delete` may already have moved the
    // row to `Deleting` without going through the `cancel` token at all (it only cancels tasks it
    // catches still `Spawning`). Re-check the persisted state before writing our own conclusion:
    // if it's already `Deleting`, tear down what we just allocated instead of clobbering that
    // with `Ready`/`Failed`, which would otherwise leak the cluster past its requested deletion
    // until the TTL reaper eventually caught it.
    let current_state = match deps.repository.get_any(&cluster.id).await {
        Ok(Some(current)) => current.state,
        Ok(None) => {
            tracing::warn!(cluster_id = %cluster.id, "cluster row vanished while spawn was completing; tearing down");
            teardown_task::teardown(&deps, &cluster).await;
            return;
        }
        Err(error) => {
            tracing::error!(cluster_id = %cluster.id, error = %error, "failed to re-check cluster state before persisting spawn outcome; proceeding with last known state");
            cluster.state.clone()
        }
    };

    if matches!(current_state, ClusterState::Deleting { .. }) {
        tracing::info!(cluster_id = %cluster.id, "cluster was deleted while spawn was completing; tearing down instead of persisting outcome");
        teardown_task::teardown(&deps, &cluster).await;
        return;
    }

    match spawn_result {
        Ok(connection) => {
            let ready_at = deps.clock.now();
            let decommission_at = ready_at + cluster.requested_ttl;
            let event = ClusterEvent::SpawnSucceeded {
                ready_at,
                decommission_at,
                connection,
            };
            match transition(&current_state, event) {
                Ok(state) => {
                    if let Err(error) = deps.repository.update_state(&cluster.id, &state).await {
                        tracing::error!(cluster_id = %cluster.id, error = %error, "failed to persist Ready state");
                    }
                }
                Err(error) => {
                    tracing::warn!(cluster_id = %cluster.id, error = %error, "cluster left Spawning before spawn completed; leaking connection info");
                }
            }
        }
        Err(error) => {
            let summary = sanitize(&error);
            tracing::warn!(cluster_id = %cluster.id, reason = %summary, "cluster spawn failed");
            let event = ClusterEvent::SpawnFailed {
                failed_at: deps.clock.now(),
                error_summary: summary,
            };
            match transition(&current_state, event) {
                Ok(state) => {
                    if let Err(error) = deps.repository.update_state(&cluster.id, &state).await {
                        tracing::error!(cluster_id = %cluster.id, error = %error, "failed to persist Failed state");
                    }
                }
                Err(error) => {
                    tracing::warn!(cluster_id = %cluster.id, error = %error, "cluster left Spawning before failure could be recorded");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{run, sanitize};
    use crate::backends::ClusterBackend;
    use crate::client_workers::{ClientWorkerError, ClientWorkers};
    use crate::domain::cluster::{Cluster, ClusterError, ClusterState, DeleteReason};
    use crate::domain::ids::{ClientId, ClusterId, WorkerUser};
    use crate::domain::service_kind::{
        ConnectionInfo, PostgresConnectionInfo, ServiceKind, ServiceSpec,
    };
    use crate::ports::clock::FakeClock;
    use crate::ports::container_runtime::{ContainerHandle, DockerError};
    use crate::ports::privileged_exec::{
        CommandOutput, PrivilegedCommand, PrivilegedExecError, PrivilegedExecutor,
    };
    use crate::ports::repository::{ClusterRepository, RepositoryError};
    use crate::redacted::Redacted;
    use crate::service::deps::TaskDeps;
    use crate::test_support::InMemoryClusterRepository;
    use async_trait::async_trait;
    use chrono::{TimeDelta, Utc};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    const OWNER: &str = "agent";

    fn client_workers(worker: Option<WorkerUser>) -> Arc<ClientWorkers> {
        let mut map = HashMap::new();
        if let Some(worker) = worker {
            map.insert(ClientId::new(OWNER), worker);
        }
        Arc::new(ClientWorkers::new(map))
    }

    struct ScriptedBackend {
        succeed: bool,
        block_forever: bool,
    }

    #[async_trait]
    impl ClusterBackend for ScriptedBackend {
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
            if self.block_forever {
                std::future::pending::<()>().await;
            }
            if self.succeed {
                Ok(ConnectionInfo::Postgres(PostgresConnectionInfo {
                    host: "127.0.0.1".to_string(),
                    port: 55432,
                    dbname: "app_salmon".to_string(),
                    user: "app_salmon".to_string(),
                    password: Redacted::new("hunter2".to_string()),
                }))
            } else {
                Err(ClusterError::BackendSpawnFailed(
                    "simulated failure".to_string(),
                ))
            }
        }

        async fn teardown(&self, _cluster_id: &ClusterId) -> Result<(), ClusterError> {
            Ok(())
        }

        async fn is_alive(&self, _cluster_id: &ClusterId) -> Result<bool, ClusterError> {
            unreachable!("spawn tests never call is_alive")
        }
    }

    struct NoopExecutor;

    #[async_trait]
    impl PrivilegedExecutor for NoopExecutor {
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

    fn spawning_cluster() -> Cluster {
        Cluster {
            id: ClusterId::new(ulid::Ulid::r#gen()),
            owner: ClientId::new("agent"),
            service: ServiceSpec {
                kind: ServiceKind::Postgres,
                pgvector: false,
            },
            requested_ttl: TimeDelta::seconds(300),
            requested_at: Utc::now(),
            state: ClusterState::Spawning {
                started_at: Utc::now(),
            },
            worker: None,
            slot: 0,
        }
    }

    fn deps_with(
        backend: Arc<dyn ClusterBackend>,
        worker: Option<WorkerUser>,
    ) -> (Arc<TaskDeps>, Arc<InMemoryClusterRepository>) {
        let repository = Arc::new(InMemoryClusterRepository::new());
        let deps = Arc::new(TaskDeps {
            repository: repository.clone(),
            client_workers: client_workers(worker),
            privileged_exec: Arc::new(NoopExecutor),
            backends: HashMap::from([(ServiceKind::Postgres, backend)]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
        });
        (deps, repository)
    }

    fn deps_with_repo(
        backend: Arc<dyn ClusterBackend>,
        worker: Option<WorkerUser>,
        repository: Arc<dyn ClusterRepository>,
    ) -> Arc<TaskDeps> {
        Arc::new(TaskDeps {
            repository,
            client_workers: client_workers(worker),
            privileged_exec: Arc::new(NoopExecutor),
            backends: HashMap::from([(ServiceKind::Postgres, backend)]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
        })
    }

    #[tokio::test]
    async fn successful_spawn_persists_ready_state() {
        let backend = Arc::new(ScriptedBackend {
            succeed: true,
            block_forever: false,
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let (deps, repository) = deps_with(backend, Some(worker));

        let cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        run(deps, cluster.clone(), CancellationToken::new()).await;

        let stored = repository
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        assert!(matches!(stored.state, ClusterState::Ready { .. }));
        assert!(stored.worker.is_some());
    }

    #[tokio::test]
    async fn failed_spawn_persists_failed_state_with_sanitized_summary() {
        let backend = Arc::new(ScriptedBackend {
            succeed: false,
            block_forever: false,
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let (deps, repository) = deps_with(backend, Some(worker));

        let cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        run(deps, cluster.clone(), CancellationToken::new()).await;

        let stored = repository
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        match stored.state {
            ClusterState::Failed { error_summary, .. } => {
                assert_eq!(error_summary, "simulated failure");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn owner_with_no_configured_account_persists_failed_state() {
        let backend = Arc::new(ScriptedBackend {
            succeed: true,
            block_forever: false,
        });
        // No client-worker mapping configured at all -> the lookup fails immediately. Not
        // reachable via the real API (the owner was authenticated against the same client list
        // this mapping is built from), but exercised defensively here.
        let (deps, repository) = deps_with(backend, None);

        let cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        run(deps, cluster.clone(), CancellationToken::new()).await;

        let stored = repository
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        match stored.state {
            ClusterState::Failed { error_summary, .. } => {
                assert_eq!(error_summary, "worker preparation failed");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancellation_mid_spawn_tears_down_and_deletes_the_row() {
        let backend = Arc::new(ScriptedBackend {
            succeed: true,
            block_forever: true,
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let (deps, repository) = deps_with(backend, Some(worker));

        let cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(run(deps.clone(), cluster.clone(), cancel_clone));

        // Give the task a moment to reach (and block inside) the backend's spawn() call, past
        // worker acquisition, before cancelling — this is what exercises "tear down whatever was
        // already allocated," not just "never started."
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancel.cancel();
        handle.await.expect("task completes after cancellation");

        assert!(
            repository
                .get_any(&cluster.id)
                .await
                .expect("query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn spawn_succeeding_after_a_concurrent_delete_tears_down_instead_of_clobbering_deleting()
    {
        // Simulates the race the cancellation token can't catch: a DELETE moves the row to
        // `Deleting` (as `ClusterService::request_delete` would) after `do_spawn` has already
        // started, without ever calling `cancel()` — e.g. because the HTTP handler observed
        // `CancelSpawn` and hasn't invoked `task_registry.cancel()` yet, or simply lost the race
        // to `do_spawn` finishing. `run` must not overwrite `Deleting` with `Ready`.
        let backend = Arc::new(ScriptedBackend {
            succeed: true,
            block_forever: false,
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let (deps, repository) = deps_with(backend, Some(worker));

        let cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");
        repository
            .update_state(
                &cluster.id,
                &ClusterState::Deleting {
                    deleting_since: Utc::now(),
                    reason: DeleteReason::UserRequested,
                },
            )
            .await
            .expect("simulate a concurrent DELETE landing first");

        run(deps.clone(), cluster.clone(), CancellationToken::new()).await;

        assert!(
            repository
                .get_any(&cluster.id)
                .await
                .expect("query")
                .is_none(),
            "row should have been torn down, not left/overwritten as Ready"
        );
    }

    #[tokio::test]
    async fn spawn_with_no_backend_registered_for_the_service_kind_persists_failed_state() {
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let repository = Arc::new(InMemoryClusterRepository::new());
        let deps = Arc::new(TaskDeps {
            repository: repository.clone(),
            client_workers: client_workers(Some(worker)),
            privileged_exec: Arc::new(NoopExecutor),
            backends: HashMap::new(),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
        });

        let cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        run(deps, cluster.clone(), CancellationToken::new()).await;

        let stored = repository
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        match stored.state {
            ClusterState::Failed { error_summary, .. } => {
                assert_eq!(error_summary, "no backend registered for this service kind");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_covers_every_docker_error_branch() {
        let container = ContainerHandle::new("app-salmon-x");
        assert_eq!(
            sanitize(&ClusterError::Docker(DockerError::HealthCheckTimeout {
                container: container.clone(),
                waited_secs: 30,
            })),
            "container did not become healthy in time"
        );
        assert_eq!(
            sanitize(&ClusterError::Docker(DockerError::ContainerNotHealthy {
                container: container.clone(),
                exit_code: Some(1),
            })),
            "container exited unexpectedly during startup"
        );
        assert_eq!(
            sanitize(&ClusterError::Docker(DockerError::StartContainer {
                container,
                source: bollard::errors::Error::DockerResponseServerError {
                    status_code: 500,
                    message: "boom".to_string(),
                },
            })),
            "container creation failed"
        );
    }

    #[test]
    fn sanitize_covers_every_client_worker_error_branch() {
        assert_eq!(
            sanitize(&ClusterError::ClientWorker(
                ClientWorkerError::UnknownClient {
                    client: ClientId::new(OWNER)
                }
            )),
            "worker preparation failed"
        );
    }

    #[test]
    fn sanitize_covers_repository_backend_and_catch_all_branches() {
        assert_eq!(
            sanitize(&ClusterError::Repository(RepositoryError::Migration(
                "boom".to_string()
            ))),
            "internal storage error"
        );
        assert_eq!(
            sanitize(&ClusterError::BackendSpawnFailed(
                "custom backend message".to_string()
            )),
            "custom backend message"
        );
        assert_eq!(
            sanitize(&ClusterError::NotFound(ClusterId::new(ulid::Ulid::nil()))),
            "spawn failed"
        );
    }

    #[tokio::test]
    async fn row_vanishing_mid_spawn_tears_down_instead_of_recreating_it() {
        // A crash-window bookkeeping gap (documented in `docs/DESIGN.md`) means a row can, in
        // theory, be gone entirely by the time a spawn task finishes rather than sitting in
        // `Deleting` — this exercises `run`'s `Ok(None)` branch, distinct from the `Deleting` race
        // covered above.
        let backend = Arc::new(ScriptedBackend {
            succeed: true,
            block_forever: false,
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let (deps, repository) = deps_with(backend, Some(worker));

        let cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");
        repository
            .delete(&cluster.id)
            .await
            .expect("simulate the row vanishing before do_spawn resolves");

        run(deps.clone(), cluster.clone(), CancellationToken::new()).await;

        assert!(
            repository
                .get_any(&cluster.id)
                .await
                .expect("query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn repository_error_while_rechecking_state_falls_back_to_the_last_known_state() {
        let backend = Arc::new(ScriptedBackend {
            succeed: true,
            block_forever: false,
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let cluster = spawning_cluster();

        // `run` calls exactly three repository methods: `set_worker` (from `do_spawn`), then
        // `get_any` to re-check state, then `update_state` to persist the outcome. Mocking lets
        // this test say precisely that, and assert `update_state`'s argument directly, rather
        // than reading storage back through a delegating fake.
        let mut repository = crate::ports::repository::MockClusterRepository::new();
        repository.expect_set_worker().returning(|_, _| Ok(()));
        repository
            .expect_get_any()
            .times(1)
            .returning(|_| Err(RepositoryError::Migration("simulated failure".to_string())));
        repository
            .expect_update_state()
            .times(1)
            .withf(|_, state| matches!(state, ClusterState::Ready { .. }))
            .returning(|_, _| Ok(()));

        let deps = deps_with_repo(backend, Some(worker), Arc::new(repository));

        run(deps, cluster, CancellationToken::new()).await;
    }

    #[tokio::test]
    async fn repository_error_while_persisting_ready_state_is_logged_not_fatal() {
        let backend = Arc::new(ScriptedBackend {
            succeed: true,
            block_forever: false,
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let cluster = spawning_cluster();
        let current = cluster.clone();

        let mut repository = crate::ports::repository::MockClusterRepository::new();
        repository.expect_set_worker().returning(|_, _| Ok(()));
        repository
            .expect_get_any()
            .times(1)
            .returning(move |_| Ok(Some(current.clone())));
        repository
            .expect_update_state()
            .times(1)
            .withf(|_, state| matches!(state, ClusterState::Ready { .. }))
            .returning(|_, _| Err(RepositoryError::Migration("simulated failure".to_string())));

        let deps = deps_with_repo(backend, Some(worker), Arc::new(repository));

        // Must not panic even though the final persist fails.
        run(deps, cluster, CancellationToken::new()).await;
    }

    #[tokio::test]
    async fn repository_error_while_persisting_failed_state_is_logged_not_fatal() {
        let backend = Arc::new(ScriptedBackend {
            succeed: false,
            block_forever: false,
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let cluster = spawning_cluster();
        let current = cluster.clone();

        let mut repository = crate::ports::repository::MockClusterRepository::new();
        repository.expect_set_worker().returning(|_, _| Ok(()));
        repository
            .expect_get_any()
            .times(1)
            .returning(move |_| Ok(Some(current.clone())));
        repository
            .expect_update_state()
            .times(1)
            .withf(|_, state| matches!(state, ClusterState::Failed { .. }))
            .returning(|_, _| Err(RepositoryError::Migration("simulated failure".to_string())));

        let deps = deps_with_repo(backend, Some(worker), Arc::new(repository));

        run(deps, cluster, CancellationToken::new()).await;
    }

    #[tokio::test]
    async fn spawn_success_racing_an_already_ready_row_does_not_clobber_it() {
        // Not reachable in practice (only `spawn_task` ever writes `Ready`), but exercises the
        // `transition` rejection branch defensively: `current_state` may be anything other than
        // `Spawning`/`Deleting` by the time we re-check it, and `run` must not panic or overwrite
        // it — it logs and leaves the row alone.
        let backend = Arc::new(ScriptedBackend {
            succeed: true,
            block_forever: false,
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let (deps, repository) = deps_with(backend, Some(worker));

        let cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");
        let ready_at = Utc::now();
        repository
            .update_state(
                &cluster.id,
                &ClusterState::Ready {
                    ready_at,
                    decommission_at: ready_at + TimeDelta::seconds(300),
                    connection: ConnectionInfo::Postgres(PostgresConnectionInfo {
                        host: "127.0.0.1".to_string(),
                        port: 55432,
                        dbname: "app_salmon".to_string(),
                        user: "app_salmon".to_string(),
                        password: Redacted::new("already-ready-secret".to_string()),
                    }),
                },
            )
            .await
            .expect("simulate the row already being Ready by the time we re-check");

        run(deps, cluster.clone(), CancellationToken::new()).await;

        let stored = repository
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        match stored.state {
            ClusterState::Ready { connection, .. } => {
                let ConnectionInfo::Postgres(connection) = connection else {
                    panic!("expected Postgres connection info");
                };
                assert_eq!(
                    connection.password.expose(),
                    "already-ready-secret",
                    "the pre-existing Ready state must not be overwritten"
                );
            }
            other => panic!("expected the pre-existing Ready state, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_failure_racing_an_already_ready_row_does_not_clobber_it() {
        let backend = Arc::new(ScriptedBackend {
            succeed: false,
            block_forever: false,
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let (deps, repository) = deps_with(backend, Some(worker));

        let cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");
        let ready_at = Utc::now();
        repository
            .update_state(
                &cluster.id,
                &ClusterState::Ready {
                    ready_at,
                    decommission_at: ready_at + TimeDelta::seconds(300),
                    connection: ConnectionInfo::Postgres(PostgresConnectionInfo {
                        host: "127.0.0.1".to_string(),
                        port: 55432,
                        dbname: "app_salmon".to_string(),
                        user: "app_salmon".to_string(),
                        password: Redacted::new("already-ready-secret".to_string()),
                    }),
                },
            )
            .await
            .expect("simulate the row already being Ready by the time we re-check");

        run(deps, cluster.clone(), CancellationToken::new()).await;

        let stored = repository
            .get_any(&cluster.id)
            .await
            .expect("query")
            .expect("row present");
        assert!(
            matches!(stored.state, ClusterState::Ready { .. }),
            "the pre-existing Ready state must not be overwritten by a Failed transition"
        );
    }
}
