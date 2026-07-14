//! Runs a cluster's backend spawn in the background after `ClusterService::create` returns.
//! Persists the outcome (`Ready` or `Failed`) itself — this is a fire-and-forget background
//! task, not something with a caller waiting on a `Result`.
//!
//! Cancellation (a `DELETE` arriving while still `Spawning`) races against the spawn work via
//! `tokio::select!`. On cancellation, this task tears down whatever it had already allocated
//! itself, by calling the same [`teardown_task::teardown`] function `teardown_task::run` uses —
//! that's what guarantees exactly one task ever tears a given cluster down: the caller that
//! cancels a spawn does not *also* start a fresh teardown task racing against this one.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::client_workers::{ClientWorkerError, worker_data_dir};
use crate::domain::cluster::{Cluster, ClusterError, ClusterEvent, ClusterState, transition};
use crate::domain::service_kind::ConnectionInfo;
use crate::domain::tar_validation::{self, TarValidationError};
use crate::ports::container_runtime::DockerError;
use crate::ports::privileged_exec::PrivilegedCommand;
use crate::service::deps::TaskDeps;
use crate::service::teardown_task;

/// The fixed name of the worker-owned subdirectory an uploaded `project_tar` is adopted into —
/// see [`crate::backends::ClusterBackend::worker_subdirs`]. A backend that accepts an uploaded
/// tree (currently only `SupabaseBackend`) must include this exact name in its declared
/// `worker_subdirs()` for [`adopt_project_tar`] to have anywhere worker-owned to copy into.
const PROJECT_SUBDIR: &str = "project";

/// Maps a [`TarValidationError`] to a caller-facing summary safe to persist/return — never the
/// error's own `Display` text, which echoes back paths taken directly from the caller's own
/// upload (not a leak of *other* callers' data, but needlessly detailed for an API response).
///
/// # Arguments
///
/// - `error`: the validation failure to summarize.
///
/// # Returns
///
/// A short, sanitized summary naming which broad category of problem was found.
fn sanitize_tar_error(error: &TarValidationError) -> String {
    match error {
        TarValidationError::DisallowedEntryType { .. } => {
            "project_tar contains an entry type that isn't allowed (only regular files and \
             directories are)"
                .to_string()
        }
        TarValidationError::EntryTooLarge { .. } | TarValidationError::TotalTooLarge { .. } => {
            "project_tar exceeds the configured size limit".to_string()
        }
        TarValidationError::UnsafeEntry { .. } => {
            "project_tar contains an entry with an unsafe path".to_string()
        }
        TarValidationError::Read { .. } | TarValidationError::Extract { .. } => {
            "project_tar could not be read or extracted".to_string()
        }
    }
}

/// Validates and extracts `tar_bytes` into an `app_salmon`-owned staging directory, then hands it
/// off to a privileged copy (see [`crate::ports::privileged_exec::PrivilegedCommand::AdoptStagedTree`])
/// so it lands worker-owned at `<worker's slot dir>/project` — the conventional subdirectory a
/// backend that wants the uploaded tree declares via `worker_subdirs()` (see [`PROJECT_SUBDIR`]).
/// Runs before any backend `spawn()` call, so a malformed upload is rejected before any container
/// is created (see `docs/DESIGN.md` §11).
///
/// The staging directory is `<tar_staging_dir_base>/<worker>/slot-<slot>` — the same
/// `<worker>/slot-<N>` shape as [`worker_data_dir`], not one keyed on `cluster_id`. This keeps the
/// set of possible staging paths bounded and enumerable in advance (one per `(worker, slot)`
/// pair), so the privileged executor's sudoers rule for `AdoptStagedTree` can be written as exact
/// literal paths, matching this project's no-wildcard sudoers convention (see
/// `scripts/setup-e2e-env.sh`) instead of needing a path built from a caller-supplied
/// `cluster_id`. Collision-free for the same reason worker/slot pairs already are: at most one
/// live cluster occupies a given slot at a time.
///
/// # Arguments
///
/// - `deps`: shared task dependencies — `tar_staging_dir_base`/`tar_limits` for validation,
///   `privileged_exec` for the worker-owned copy.
/// - `worker`: the worker account to run the privileged copy as; also selects the staging
///   subdirectory.
/// - `slot`: the cluster's slot number; also selects the staging subdirectory.
/// - `dest`: the pre-existing, worker-owned destination directory to copy the extracted tree
///   into — must already exist (via a prior `PrepareWorkerDir`).
/// - `tar_bytes`: the raw, not-yet-validated tar archive bytes.
///
/// # Returns
///
/// Nothing, on success — the extracted tree is now at `dest`, and the staging directory has been
/// removed.
///
/// # Errors
///
/// [`ClusterError::BackendSpawnFailed`] if the staging directory can't be created, `tar_bytes`
/// fails validation (see [`sanitize_tar_error`]), or the extraction task itself panics; otherwise
/// propagates whatever [`ClientWorkerError`] the privileged copy step produces. The staging
/// directory is removed on a best-effort basis regardless of outcome (a cleanup failure is logged,
/// not fatal — matching this project's established "log, don't fail the caller over cleanup"
/// convention, e.g. `teardown_task`'s wipe-failure handling).
async fn adopt_project_tar(
    deps: &TaskDeps,
    worker: &crate::domain::ids::WorkerUser,
    slot: u32,
    dest: &std::path::Path,
    tar_bytes: &[u8],
) -> Result<(), ClusterError> {
    let staging = worker_data_dir(&deps.tar_staging_dir_base, worker, slot);
    tokio::fs::create_dir_all(&staging)
        .await
        .map_err(|_source| {
            ClusterError::BackendSpawnFailed("failed to prepare staging directory".to_string())
        })?;

    // The later `AdoptStagedTree` copy runs as `worker`, a different uid than this
    // (app_salmon-owned) process — it must be able to traverse every ancestor directory we just
    // created to reach the extracted files. Set the mode explicitly rather than trusting this
    // process's umask, which may be more restrictive than world-traversable in some deployments.
    for ancestor in [
        deps.tar_staging_dir_base.join(worker.as_str()),
        staging.clone(),
    ] {
        tokio::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o755))
            .await
            .map_err(|_source| {
                ClusterError::BackendSpawnFailed(
                    "failed to prepare staging directory permissions".to_string(),
                )
            })?;
    }

    let limits = deps.tar_limits;
    let tar_bytes = tar_bytes.to_vec();
    let staging_for_extract = staging.clone();
    let extract_result = tokio::task::spawn_blocking(move || {
        tar_validation::validate_and_extract(&tar_bytes, &staging_for_extract, &limits)
    })
    .await;

    let outcome = match extract_result {
        Ok(Ok(())) => deps
            .privileged_exec
            .run_as(
                worker,
                PrivilegedCommand::AdoptStagedTree {
                    staging_path: staging.display().to_string(),
                    dest_path: dest.display().to_string(),
                },
            )
            .await
            .map(|_output| ())
            .map_err(ClientWorkerError::Prepare)
            .map_err(ClusterError::from),
        Ok(Err(validation_error)) => Err(ClusterError::BackendSpawnFailed(sanitize_tar_error(
            &validation_error,
        ))),
        Err(_join_error) => Err(ClusterError::BackendSpawnFailed(
            "internal error while extracting project_tar".to_string(),
        )),
    };

    if let Err(error) = tokio::fs::remove_dir_all(&staging).await {
        tracing::warn!(worker = %worker.as_str(), slot, error = %error, "failed to remove tar staging directory; leaking disk space, not failing the spawn over it");
    }

    outcome
}

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
/// - `project_tar`: the raw bytes of a caller-uploaded project tree (see `http::handlers`'s
///   `multipart/form-data` path), if any — kind-agnostic here (any backend that declares
///   `worker_subdirs()` including [`PROJECT_SUBDIR`] can receive one; currently only
///   `SupabaseBackend` does).
///
/// # Returns
///
/// Connection details for the newly spawned backend resource, once it's up.
///
/// # Errors
///
/// Returns [`ClusterError::BackendSpawnFailed`] if no backend is registered for the cluster's
/// service kind, if `project_tar` is present but fails validation (see [`adopt_project_tar`]), a
/// [`crate::client_workers::ClientWorkerError`] (via `#[from]`) if the owner has no configured
/// account or a privileged directory-preparation/adopt command fails, or whatever error the
/// backend's own `spawn` call produces (including [`ClusterError::Repository`] if persisting the
/// resolved worker fails).
async fn do_spawn(
    deps: &TaskDeps,
    cluster: &mut Cluster,
    project_tar: Option<&[u8]>,
) -> Result<ConnectionInfo, ClusterError> {
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

    let base = worker_data_dir(&deps.worker_data_dir_base, &worker, cluster.slot);
    let subdirs = backend.worker_subdirs();
    let prepare_targets: Vec<std::path::PathBuf> = if subdirs.is_empty() {
        vec![base.clone()]
    } else {
        subdirs.iter().map(|subdir| base.join(subdir)).collect()
    };
    for target in &prepare_targets {
        deps.privileged_exec
            .run_as(
                &worker,
                PrivilegedCommand::PrepareWorkerDir {
                    path: target.display().to_string(),
                },
            )
            .await
            .map_err(ClientWorkerError::Prepare)?;
    }

    if let Some(tar_bytes) = project_tar {
        adopt_project_tar(
            deps,
            &worker,
            cluster.slot,
            &base.join(PROJECT_SUBDIR),
            tar_bytes,
        )
        .await?;
    }

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
/// - `project_tar`: the raw bytes of a caller-uploaded project tree, if any — see [`do_spawn`].
pub async fn run(
    deps: Arc<TaskDeps>,
    mut cluster: Cluster,
    cancel: CancellationToken,
    project_tar: Option<Vec<u8>>,
) {
    let outcome = tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        result = do_spawn(&deps, &mut cluster, project_tar.as_deref()) => Some(result),
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
    use super::{do_spawn, run, sanitize};
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

    /// A backend that declares configurable `worker_subdirs()` and always spawns successfully —
    /// used to test `do_spawn`'s subdirectory-preparation and tar-adoption orchestration in
    /// isolation from `ScriptedBackend`'s (default, no-subdirs) existing test coverage.
    struct SubdirDeclaringBackend {
        subdirs: &'static [&'static str],
    }

    #[async_trait]
    impl ClusterBackend for SubdirDeclaringBackend {
        fn kind(&self) -> ServiceKind {
            ServiceKind::Postgres
        }

        fn worker_subdirs(&self) -> &[&'static str] {
            self.subdirs
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
                password: Redacted::new("hunter2".to_string()),
            }))
        }

        async fn teardown(&self, _cluster_id: &ClusterId) -> Result<(), ClusterError> {
            Ok(())
        }

        async fn is_alive(&self, _cluster_id: &ClusterId) -> Result<bool, ClusterError> {
            unreachable!("these tests never call is_alive")
        }
    }

    /// Records every `PrivilegedCommand` it's asked to run, in order, and always succeeds —
    /// used to assert exactly which privileged operations `do_spawn` issues and with what
    /// arguments, rather than just that it doesn't error.
    #[derive(Default)]
    struct RecordingExecutor {
        calls: std::sync::Mutex<Vec<PrivilegedCommand>>,
    }

    impl RecordingExecutor {
        fn calls(&self) -> Vec<PrivilegedCommand> {
            self.calls.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl PrivilegedExecutor for RecordingExecutor {
        async fn run_as(
            &self,
            _worker: &WorkerUser,
            command: PrivilegedCommand,
        ) -> Result<CommandOutput, PrivilegedExecError> {
            self.calls.lock().expect("lock").push(command);
            Ok(CommandOutput {
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    /// Appends one entry to `builder` with a well-formed header — mirrors
    /// `domain::tar_validation`'s own test helper of the same shape.
    fn append_tar_entry(
        builder: &mut tar::Builder<Vec<u8>>,
        path: &str,
        entry_type: tar::EntryType,
        content: &[u8],
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).expect("set path");
        header.set_entry_type(entry_type);
        header.set_size(content.len() as u64);
        header.set_mode(if entry_type == tar::EntryType::Directory {
            0o755
        } else {
            0o644
        });
        header.set_cksum();
        builder.append(&header, content).expect("append entry");
    }

    /// Builds a minimal, well-formed in-memory tar containing a `functions/index.ts` entry (and
    /// its parent directory entry, required since extraction doesn't auto-create missing
    /// parents) — enough for [`tar_validation::validate_and_extract`] to accept it.
    fn sample_tar_bytes() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        append_tar_entry(&mut builder, "functions", tar::EntryType::Directory, b"");
        append_tar_entry(
            &mut builder,
            "functions/index.ts",
            tar::EntryType::Regular,
            b"export default {}",
        );
        builder.into_inner().expect("finish tar")
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
            tar_staging_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/tar-staging"),
            tar_limits: crate::domain::tar_validation::TarLimits {
                max_entry_bytes: 10_485_760,
                max_total_bytes: 52_428_800,
            },
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
            tar_staging_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/tar-staging"),
            tar_limits: crate::domain::tar_validation::TarLimits {
                max_entry_bytes: 10_485_760,
                max_total_bytes: 52_428_800,
            },
        })
    }

    #[tokio::test]
    async fn do_spawn_prepares_only_the_slot_dir_when_backend_declares_no_subdirs() {
        let backend = Arc::new(ScriptedBackend {
            succeed: true,
            block_forever: false,
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let repository = Arc::new(InMemoryClusterRepository::new());
        let executor = Arc::new(RecordingExecutor::default());
        let deps = Arc::new(TaskDeps {
            repository: repository.clone(),
            client_workers: client_workers(Some(worker.clone())),
            privileged_exec: executor.clone(),
            backends: HashMap::from([(ServiceKind::Postgres, backend as Arc<dyn ClusterBackend>)]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            tar_staging_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/tar-staging"),
            tar_limits: crate::domain::tar_validation::TarLimits {
                max_entry_bytes: 10_485_760,
                max_total_bytes: 52_428_800,
            },
        });
        let mut cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        do_spawn(&deps, &mut cluster, None)
            .await
            .expect("spawn succeeds");

        let calls = executor.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            PrivilegedCommand::PrepareWorkerDir {
                path: "/var/lib/app_salmon/workers/salmon-worker-00/slot-0".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn do_spawn_prepares_each_declared_worker_subdir() {
        let backend = Arc::new(SubdirDeclaringBackend {
            subdirs: &["project"],
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let repository = Arc::new(InMemoryClusterRepository::new());
        let executor = Arc::new(RecordingExecutor::default());
        let deps = Arc::new(TaskDeps {
            repository: repository.clone(),
            client_workers: client_workers(Some(worker.clone())),
            privileged_exec: executor.clone(),
            backends: HashMap::from([(ServiceKind::Postgres, backend as Arc<dyn ClusterBackend>)]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            tar_staging_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/tar-staging"),
            tar_limits: crate::domain::tar_validation::TarLimits {
                max_entry_bytes: 10_485_760,
                max_total_bytes: 52_428_800,
            },
        });
        let mut cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        do_spawn(&deps, &mut cluster, None)
            .await
            .expect("spawn succeeds");

        let calls = executor.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            PrivilegedCommand::PrepareWorkerDir {
                path: "/var/lib/app_salmon/workers/salmon-worker-00/slot-0/project".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn do_spawn_adopts_a_valid_project_tar_before_calling_backend_spawn() {
        let backend = Arc::new(SubdirDeclaringBackend {
            subdirs: &["project"],
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let repository = Arc::new(InMemoryClusterRepository::new());
        let executor = Arc::new(RecordingExecutor::default());
        let staging_root = tempfile::tempdir().expect("tempdir");
        let deps = Arc::new(TaskDeps {
            repository: repository.clone(),
            client_workers: client_workers(Some(worker.clone())),
            privileged_exec: executor.clone(),
            backends: HashMap::from([(ServiceKind::Postgres, backend as Arc<dyn ClusterBackend>)]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            tar_staging_dir_base: staging_root.path().to_path_buf(),
            tar_limits: crate::domain::tar_validation::TarLimits {
                max_entry_bytes: 10_485_760,
                max_total_bytes: 52_428_800,
            },
        });
        let mut cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        do_spawn(&deps, &mut cluster, Some(&sample_tar_bytes()))
            .await
            .expect("spawn succeeds");

        let calls = executor.calls();
        assert_eq!(
            calls.len(),
            2,
            "expected a PrepareWorkerDir then an AdoptStagedTree"
        );
        let expected_staging = staging_root.path().join("salmon-worker-00").join("slot-0");
        assert_eq!(
            calls[1],
            PrivilegedCommand::AdoptStagedTree {
                staging_path: expected_staging.display().to_string(),
                dest_path: "/var/lib/app_salmon/workers/salmon-worker-00/slot-0/project"
                    .to_string(),
            }
        );
        assert!(
            !expected_staging.exists(),
            "staging directory should be cleaned up after a successful adopt"
        );
    }

    #[tokio::test]
    async fn do_spawn_rejects_an_invalid_project_tar_without_calling_backend_spawn() {
        let backend = Arc::new(SubdirDeclaringBackend {
            subdirs: &["project"],
        });
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let repository = Arc::new(InMemoryClusterRepository::new());
        let executor = Arc::new(RecordingExecutor::default());
        let staging_root = tempfile::tempdir().expect("tempdir");
        let deps = Arc::new(TaskDeps {
            repository: repository.clone(),
            client_workers: client_workers(Some(worker.clone())),
            privileged_exec: executor.clone(),
            backends: HashMap::from([(ServiceKind::Postgres, backend as Arc<dyn ClusterBackend>)]),
            clock: Arc::new(FakeClock::new(Utc::now())),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            tar_staging_dir_base: staging_root.path().to_path_buf(),
            tar_limits: crate::domain::tar_validation::TarLimits {
                max_entry_bytes: 10_485_760,
                max_total_bytes: 52_428_800,
            },
        });
        let mut cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        let err = do_spawn(&deps, &mut cluster, Some(b"this is not a tar file"))
            .await
            .expect_err("malformed tar is rejected");

        assert!(matches!(err, ClusterError::BackendSpawnFailed(_)));
        // Only the PrepareWorkerDir call happened — extraction failed before AdoptStagedTree
        // (and so before `backend.spawn()`, which `SubdirDeclaringBackend` would otherwise
        // always succeed at) was ever reached.
        let calls = executor.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            calls[0],
            PrivilegedCommand::PrepareWorkerDir { .. }
        ));
        assert!(
            !staging_root
                .path()
                .join("salmon-worker-00")
                .join("slot-0")
                .exists(),
            "staging directory should be cleaned up even after a failed extraction"
        );
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

        run(deps, cluster.clone(), CancellationToken::new(), None).await;

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

        run(deps, cluster.clone(), CancellationToken::new(), None).await;

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

        run(deps, cluster.clone(), CancellationToken::new(), None).await;

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
        let handle = tokio::spawn(run(deps.clone(), cluster.clone(), cancel_clone, None));

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

        run(
            deps.clone(),
            cluster.clone(),
            CancellationToken::new(),
            None,
        )
        .await;

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
            tar_staging_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/tar-staging"),
            tar_limits: crate::domain::tar_validation::TarLimits {
                max_entry_bytes: 10_485_760,
                max_total_bytes: 52_428_800,
            },
        });

        let cluster = spawning_cluster();
        repository
            .try_insert_if_under_quota(&cluster, 10)
            .await
            .expect("seed row");

        run(deps, cluster.clone(), CancellationToken::new(), None).await;

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

        run(
            deps.clone(),
            cluster.clone(),
            CancellationToken::new(),
            None,
        )
        .await;

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

        run(deps, cluster, CancellationToken::new(), None).await;
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
        run(deps, cluster, CancellationToken::new(), None).await;
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

        run(deps, cluster, CancellationToken::new(), None).await;
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

        run(deps, cluster.clone(), CancellationToken::new(), None).await;

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

        run(deps, cluster.clone(), CancellationToken::new(), None).await;

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
