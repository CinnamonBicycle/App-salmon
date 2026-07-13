//! Phase 1's only real [`ClusterBackend`]: a single Postgres container (optionally with the
//! `pgvector` extension enabled), bind-mounted into a specific worker's data directory and run
//! as that worker's uid/gid.
//!
//! Readiness is a Docker `HEALTHCHECK` (`pg_isready`, configured on the [`ContainerSpec`] this
//! backend submits) polled via `ContainerRuntime::inspect`'s `health` field — not this backend
//! dialing Postgres itself with `tokio_postgres::connect`. Docker already tracks "is the
//! process inside this container answering" for any healthcheck-bearing container; duplicating
//! that as an application-level wire-protocol probe was redundant, and meant the happy path could
//! only be tested against a real Postgres server (see `docs/DESIGN.md`'s testing notes for the
//! prior design and why it changed). `connect()` still exists for exactly one purpose: enabling
//! `pgvector` (`CREATE EXTENSION IF NOT EXISTS vector`) once the healthcheck confirms Postgres is
//! actually accepting connections — that's the one place this backend does something that could
//! be read as "initializing schema"; it's in scope because the caller explicitly asked for it via
//! `ServiceSpec::pgvector`, and it's the only way to make the extension available (App Salmon
//! does not, and will not, run application migrations/table setup — see `docs/DESIGN.md`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::backends::{ClusterBackend, health_wait};
use crate::client_workers::worker_data_dir;
use crate::domain::cluster::ClusterError;
use crate::domain::ids::{ClusterId, WorkerUser};
use crate::domain::service_kind::{
    ConnectionInfo, PostgresConnectionInfo, ServiceKind, ServiceSpec,
};
use crate::ports::container_runtime::{
    BindMount, ContainerHandle, ContainerRuntime, ContainerSpec, ContainerStatus, HealthCheck,
    OciRuntime,
};
use crate::ports::secrets::SecretGenerator;
use crate::redacted::Redacted;

const CONTAINER_PORT: u16 = 5432;
const DB_USER: &str = "app_salmon";
const DB_NAME: &str = "app_salmon";
const PASSWORD_LEN: usize = 32;
const CLUSTER_ID_LABEL: &str = "app_salmon.cluster_id";
/// How often Docker itself runs `pg_isready` inside the container — independent of
/// `HEALTH_POLL_INTERVAL`, which is how often *this backend* asks Docker for the current status.
const HEALTHCHECK_INTERVAL: Duration = Duration::from_secs(1);
const HEALTHCHECK_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const HEALTHCHECK_RETRIES: u32 = 3;

/// Computes the deterministic container name for `cluster_id`, so teardown can find the container
/// again without any extra persisted lookup state.
///
/// # Arguments
///
/// - `cluster_id`: the cluster to name a container for.
///
/// # Returns
///
/// The container name to create/look up, always of the form `app-salmon-<cluster_id>`.
fn container_name(cluster_id: &ClusterId) -> String {
    format!("app-salmon-{cluster_id}")
}

pub struct PostgresBackend {
    /// How this backend actually creates/inspects/removes containers.
    container_runtime: Arc<dyn ContainerRuntime>,
    /// Source of the per-cluster generated DB password.
    secrets: Arc<dyn SecretGenerator>,
    /// The Postgres (or `pgvector`) image to run, e.g. `pgvector/pgvector:pg16`.
    image: String,
    /// Base directory under which each worker's bind-mounted data directory lives.
    worker_data_dir_base: PathBuf,
    /// The overall deadline `wait_until_ready` polls against — bounds how long a caller waits
    /// regardless of how Docker's own `HEALTHCHECK` interval/timeout/retries are configured.
    health_check_timeout: Duration,
}

impl PostgresBackend {
    /// Builds a `PostgresBackend` from its dependencies and configuration.
    ///
    /// # Arguments
    ///
    /// - `container_runtime`: how to create/inspect/remove containers.
    /// - `secrets`: source of the per-cluster generated DB password.
    /// - `image`: the Postgres (or `pgvector`) image to run.
    /// - `worker_data_dir_base`: base directory under which each worker's bind-mounted data
    ///   directory lives.
    /// - `health_check_timeout`: the overall deadline `wait_until_ready` polls against.
    ///
    /// # Returns
    ///
    /// A ready-to-use `PostgresBackend`.
    #[must_use]
    pub fn new(
        container_runtime: Arc<dyn ContainerRuntime>,
        secrets: Arc<dyn SecretGenerator>,
        image: String,
        worker_data_dir_base: PathBuf,
        health_check_timeout: Duration,
    ) -> Self {
        Self {
            container_runtime,
            secrets,
            image,
            worker_data_dir_base,
            health_check_timeout,
        }
    }

    /// Polls `inspect` (via the shared [`health_wait::wait_until_healthy`]) until the container
    /// reports a published port and Docker's own `HEALTHCHECK` (`pg_isready`, set on the
    /// [`ContainerSpec`] this backend submits) reports healthy.
    ///
    /// # Arguments
    ///
    /// - `handle`: the container to poll.
    ///
    /// # Returns
    ///
    /// The host port Postgres is reachable on.
    ///
    /// # Errors
    ///
    /// Whatever [`health_wait::wait_until_healthy`] returns, plus
    /// [`ClusterError::BackendSpawnFailed`] in the (should-never-happen, since this backend
    /// always requests a published port) case where the container became healthy without one.
    async fn wait_until_ready(&self, handle: &ContainerHandle) -> Result<u16, ClusterError> {
        health_wait::wait_until_healthy(
            self.container_runtime.as_ref(),
            handle,
            self.health_check_timeout,
        )
        .await?
        .ok_or_else(|| {
            ClusterError::BackendSpawnFailed(
                "container reported healthy but published no port".to_string(),
            )
        })
    }
}

/// Opens a real `tokio_postgres` connection to a container's published port, on behalf of this
/// backend itself (currently only to run `CREATE EXTENSION IF NOT EXISTS vector` for `pgvector` —
/// see the module docs for why this is the one place readiness is still confirmed by actually
/// dialing Postgres). Spawns a background task to drive the connection, since `tokio_postgres`
/// requires that to happen for the returned `Client` to work at all.
///
/// # Arguments
///
/// - `host_port`: the host port Postgres is published on (from a `ContainerStatus::Running` with
///   `published_port: Some(_)`).
/// - `password`: the DB password to authenticate with.
///
/// # Returns
///
/// A `Client` usable to run queries once connected.
///
/// # Errors
///
/// Whatever `tokio_postgres::connect` itself returns — most commonly a connection failure or
/// authentication failure.
async fn connect(
    host_port: u16,
    password: &str,
) -> Result<tokio_postgres::Client, tokio_postgres::Error> {
    let config = format!(
        "host=127.0.0.1 port={host_port} user={DB_USER} password={password} dbname={DB_NAME} connect_timeout=2"
    );
    let (client, connection) = tokio_postgres::connect(&config, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        // Driving the connection is required for `client` to work at all; a connection error
        // here just means the client's queries will start failing, which callers already handle.
        let _ = connection.await;
    });
    Ok(client)
}

#[async_trait]
impl ClusterBackend for PostgresBackend {
    /// # Returns
    ///
    /// Always [`ServiceKind::Postgres`] — this backend only ever handles the Postgres kind.
    fn kind(&self) -> ServiceKind {
        ServiceKind::Postgres
    }

    /// Creates a Postgres container bind-mounted into `worker`'s data directory, run as
    /// `worker`'s uid/gid, waits for Docker's `HEALTHCHECK` to report `healthy` (see
    /// [`Self::wait_until_ready`]), and — if requested — enables `pgvector`.
    ///
    /// # Arguments
    ///
    /// - `cluster_id`: the cluster this container is being provisioned for; used to name the
    ///   container deterministically and to label it.
    /// - `worker`: the pre-allocated worker account the container runs as and whose data
    ///   directory it's bind-mounted into.
    /// - `slot`: `cluster_id`'s assigned directory slot, used together with `worker` to compute
    ///   the bind-mounted data directory.
    /// - `service`: the requested service configuration — only `service.pgvector` is consulted
    ///   here (`service.kind` is assumed to already be [`ServiceKind::Postgres`]).
    ///
    /// # Returns
    ///
    /// A [`ConnectionInfo`] with the generated credentials and the host/port to connect on.
    ///
    /// # Errors
    ///
    /// A [`ClusterError`] if container creation fails, the container never becomes healthy within
    /// `health_check_timeout` (see [`Self::wait_until_ready`]), or — when `service.pgvector` is
    /// set — connecting to enable the extension or running `CREATE EXTENSION` itself fails.
    async fn spawn(
        &self,
        cluster_id: &ClusterId,
        worker: &WorkerUser,
        slot: u32,
        service: &ServiceSpec,
    ) -> Result<ConnectionInfo, ClusterError> {
        let password = self.secrets.db_password(PASSWORD_LEN);
        let host_path = worker_data_dir(&self.worker_data_dir_base, worker, slot);
        let mut labels = HashMap::with_capacity(1);
        labels.insert(CLUSTER_ID_LABEL.to_string(), cluster_id.to_string());

        let spec = ContainerSpec {
            name: container_name(cluster_id),
            image: self.image.clone(),
            env: vec![
                ("POSTGRES_USER".to_string(), DB_USER.to_string()),
                ("POSTGRES_DB".to_string(), DB_NAME.to_string()),
                ("POSTGRES_PASSWORD".to_string(), password.clone()),
            ],
            labels,
            host_port: None,
            container_port: CONTAINER_PORT,
            bind_mount: Some(BindMount {
                host_path: host_path.display().to_string(),
                container_path: "/var/lib/postgresql/data".to_string(),
            }),
            run_as: Some((worker.uid(), worker.gid())),
            health_check: Some(HealthCheck {
                test: vec![
                    "CMD-SHELL".to_string(),
                    format!("pg_isready -U {DB_USER} -d {DB_NAME}"),
                ],
                interval: HEALTHCHECK_INTERVAL,
                timeout: HEALTHCHECK_PROBE_TIMEOUT,
                retries: HEALTHCHECK_RETRIES,
            }),
            runtime: OciRuntime::Runc,
            network: None,
        };

        let handle = self.container_runtime.create_and_start(&spec).await?;
        let host_port = self.wait_until_ready(&handle).await?;

        if service.pgvector {
            let client = connect(host_port, &password).await.map_err(|_source| {
                ClusterError::BackendSpawnFailed("failed to connect to enable pgvector".to_string())
            })?;
            client
                .execute("CREATE EXTENSION IF NOT EXISTS vector", &[])
                .await
                .map_err(|_source| {
                    ClusterError::BackendSpawnFailed(
                        "failed to enable pgvector extension".to_string(),
                    )
                })?;
        }

        Ok(ConnectionInfo::Postgres(PostgresConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: host_port,
            dbname: DB_NAME.to_string(),
            user: DB_USER.to_string(),
            password: Redacted::new(password),
        }))
    }

    /// Stops and removes the container `spawn` created for `cluster_id`, recomputing its name
    /// deterministically rather than needing it persisted anywhere.
    ///
    /// # Arguments
    ///
    /// - `cluster_id`: the cluster whose container should be torn down.
    ///
    /// # Returns
    ///
    /// Nothing, on success.
    ///
    /// # Errors
    ///
    /// A [`ClusterError`] if the underlying `stop_and_remove` call fails (removing an
    /// already-absent container is treated as success by the runtime layer, not an error here).
    async fn teardown(&self, cluster_id: &ClusterId) -> Result<(), ClusterError> {
        let handle = ContainerHandle::new(container_name(cluster_id));
        self.container_runtime.stop_and_remove(&handle).await?;
        Ok(())
    }

    /// Checks whether `cluster_id`'s container still exists and is running, by inspecting it —
    /// does not check Postgres's own health status, only that the container process is up.
    ///
    /// # Arguments
    ///
    /// - `cluster_id`: the cluster whose container should be checked.
    ///
    /// # Returns
    ///
    /// `true` if the container is `Running` (in any health state); `false` if it has exited or
    /// can't be found.
    ///
    /// # Errors
    ///
    /// A [`ClusterError`] if the underlying `inspect` call itself fails.
    async fn is_alive(&self, cluster_id: &ClusterId) -> Result<bool, ClusterError> {
        let handle = ContainerHandle::new(container_name(cluster_id));
        match self.container_runtime.inspect(&handle).await? {
            ContainerStatus::Running { .. } => Ok(true),
            ContainerStatus::Exited { .. } | ContainerStatus::NotFound => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PostgresBackend, container_name};
    use crate::backends::ClusterBackend;
    use crate::domain::ids::{ClusterId, WorkerUser};
    use crate::domain::service_kind::{ConnectionInfo, ServiceKind, ServiceSpec};
    use crate::ports::container_runtime::{
        ContainerHandle, ContainerRuntime, ContainerSpec, ContainerStatus, DockerError,
    };
    use crate::ports::secrets::SecretGenerator;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Default)]
    struct FakeContainerRuntime {
        created: Mutex<Vec<ContainerSpec>>,
        removed: Mutex<Vec<ContainerHandle>>,
        inspect_status: Mutex<Option<ContainerStatus>>,
        fail_create: bool,
    }

    #[async_trait]
    impl ContainerRuntime for FakeContainerRuntime {
        async fn create_and_start(
            &self,
            spec: &ContainerSpec,
        ) -> Result<ContainerHandle, DockerError> {
            if self.fail_create {
                return Err(DockerError::CreateContainer {
                    source: bollard::errors::Error::DockerResponseServerError {
                        status_code: 500,
                        message: "boom".to_string(),
                    },
                });
            }
            self.created.lock().expect("lock").push(spec.clone());
            Ok(ContainerHandle::new(spec.name.clone()))
        }

        async fn inspect(&self, _handle: &ContainerHandle) -> Result<ContainerStatus, DockerError> {
            Ok(self
                .inspect_status
                .lock()
                .expect("lock")
                .unwrap_or(ContainerStatus::Running {
                    published_port: None,
                    health: None,
                }))
        }

        async fn stop_and_remove(&self, handle: &ContainerHandle) -> Result<(), DockerError> {
            self.removed.lock().expect("lock").push(handle.clone());
            Ok(())
        }

        async fn create_network(
            &self,
            name: &str,
        ) -> Result<crate::ports::container_runtime::NetworkHandle, DockerError> {
            // PostgresBackend never requests a network attachment — unexercised by design here.
            Ok(crate::ports::container_runtime::NetworkHandle::new(name))
        }

        async fn remove_network(
            &self,
            _handle: &crate::ports::container_runtime::NetworkHandle,
        ) -> Result<(), DockerError> {
            Ok(())
        }
    }

    struct FixedSecretGenerator;

    impl SecretGenerator for FixedSecretGenerator {
        fn cluster_id(&self) -> ClusterId {
            ClusterId::new(ulid::Ulid::nil())
        }

        fn db_password(&self, len: usize) -> String {
            "p".repeat(len)
        }
    }

    fn backend(runtime: FakeContainerRuntime) -> PostgresBackend {
        PostgresBackend::new(
            std::sync::Arc::new(runtime),
            std::sync::Arc::new(FixedSecretGenerator),
            "pgvector/pgvector:pg16".to_string(),
            std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            Duration::from_millis(50),
        )
    }

    #[test]
    fn kind_is_postgres() {
        let backend = backend(FakeContainerRuntime::default());
        assert_eq!(backend.kind(), ServiceKind::Postgres);
    }

    #[test]
    fn container_name_is_deterministic_from_cluster_id() {
        let id = ClusterId::new(ulid::Ulid::nil());
        assert_eq!(container_name(&id), format!("app-salmon-{id}"));
        // Recomputing from the same id gives the same name every time, which is what lets
        // teardown find the container without any extra persisted lookup state.
        assert_eq!(container_name(&id), container_name(&id));
    }

    #[tokio::test]
    async fn spawn_propagates_create_container_failure() {
        let runtime = FakeContainerRuntime {
            fail_create: true,
            ..Default::default()
        };
        let backend = backend(runtime);
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let service = ServiceSpec {
            kind: ServiceKind::Postgres,
            pgvector: false,
        };
        let err = backend
            .spawn(&ClusterId::new(ulid::Ulid::nil()), &worker, 0, &service)
            .await
            .expect_err("create failure propagates");
        assert!(matches!(
            err,
            crate::domain::cluster::ClusterError::Docker(_)
        ));
    }

    #[tokio::test]
    async fn spawn_times_out_when_container_never_publishes_a_port() {
        let runtime = FakeContainerRuntime {
            inspect_status: Mutex::new(Some(ContainerStatus::Running {
                published_port: None,
                health: None,
            })),
            ..Default::default()
        };
        let backend = backend(runtime);
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let service = ServiceSpec {
            kind: ServiceKind::Postgres,
            pgvector: false,
        };
        let err = backend
            .spawn(&ClusterId::new(ulid::Ulid::nil()), &worker, 0, &service)
            .await
            .expect_err("never publishes a port, so it can never connect");
        assert!(matches!(
            err,
            crate::domain::cluster::ClusterError::Docker(DockerError::HealthCheckTimeout { .. })
        ));
    }

    #[tokio::test]
    async fn spawn_fails_if_healthy_but_no_port_was_ever_published() {
        // Should never happen in practice (this backend always requests a published port), but
        // `wait_until_healthy`'s success condition doesn't itself require one — this exercises
        // `wait_until_ready`'s own defensive `ok_or_else` for that combination.
        let runtime = FakeContainerRuntime {
            inspect_status: Mutex::new(Some(ContainerStatus::Running {
                published_port: None,
                health: Some(crate::ports::container_runtime::HealthState::Healthy),
            })),
            ..Default::default()
        };
        let backend = backend(runtime);
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let service = ServiceSpec {
            kind: ServiceKind::Postgres,
            pgvector: false,
        };
        let err = backend
            .spawn(&ClusterId::new(ulid::Ulid::nil()), &worker, 0, &service)
            .await
            .expect_err("healthy with no published port is still a failure");
        assert!(matches!(
            err,
            crate::domain::cluster::ClusterError::BackendSpawnFailed(_)
        ));
    }

    #[tokio::test]
    async fn spawn_times_out_when_the_healthcheck_never_reports_healthy() {
        // Published port available immediately, but the healthcheck is stuck `unhealthy` —
        // distinct from `spawn_times_out_when_container_never_publishes_a_port` above, and the
        // scenario the redesign (see module docs) specifically has to not fail-fast on: Docker
        // itself can still recover an `unhealthy` container to `healthy` on a later check, so
        // this backend keeps polling rather than treating one bad observation as final.
        let runtime = FakeContainerRuntime {
            inspect_status: Mutex::new(Some(ContainerStatus::Running {
                published_port: Some(55432),
                health: Some(crate::ports::container_runtime::HealthState::Unhealthy),
            })),
            ..Default::default()
        };
        let backend = backend(runtime);
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let service = ServiceSpec {
            kind: ServiceKind::Postgres,
            pgvector: false,
        };
        let err = backend
            .spawn(&ClusterId::new(ulid::Ulid::nil()), &worker, 0, &service)
            .await
            .expect_err("healthcheck never reports healthy, so it can never become ready");
        assert!(matches!(
            err,
            crate::domain::cluster::ClusterError::Docker(DockerError::HealthCheckTimeout { .. })
        ));
    }

    #[tokio::test]
    async fn spawn_succeeds_once_the_healthcheck_reports_healthy() {
        // The success path: no real Postgres wire-protocol handshake required, since readiness
        // is now purely a Docker `inspect()` health-status observation.
        let runtime = FakeContainerRuntime {
            inspect_status: Mutex::new(Some(ContainerStatus::Running {
                published_port: Some(55432),
                health: Some(crate::ports::container_runtime::HealthState::Healthy),
            })),
            ..Default::default()
        };
        let backend = backend(runtime);
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let service = ServiceSpec {
            kind: ServiceKind::Postgres,
            pgvector: false,
        };
        let connection = backend
            .spawn(&ClusterId::new(ulid::Ulid::nil()), &worker, 0, &service)
            .await
            .expect("healthy container is a successful spawn");
        let ConnectionInfo::Postgres(connection) = connection else {
            panic!("expected Postgres connection info");
        };
        assert_eq!(connection.port, 55432);
        assert_eq!(connection.host, "127.0.0.1");
        assert_eq!(connection.dbname, "app_salmon");
        assert_eq!(connection.user, "app_salmon");
    }

    #[tokio::test]
    async fn spawn_builds_container_spec_with_a_pg_isready_healthcheck() {
        let runtime = std::sync::Arc::new(FakeContainerRuntime {
            inspect_status: Mutex::new(Some(ContainerStatus::Running {
                published_port: Some(55432),
                health: Some(crate::ports::container_runtime::HealthState::Healthy),
            })),
            ..Default::default()
        });
        let backend = PostgresBackend::new(
            runtime.clone() as std::sync::Arc<dyn ContainerRuntime>,
            std::sync::Arc::new(FixedSecretGenerator),
            "pgvector/pgvector:pg16".to_string(),
            std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            Duration::from_millis(50),
        );
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let service = ServiceSpec {
            kind: ServiceKind::Postgres,
            pgvector: false,
        };
        backend
            .spawn(&ClusterId::new(ulid::Ulid::nil()), &worker, 0, &service)
            .await
            .expect("healthy container is a successful spawn");

        let created = runtime.created.lock().expect("lock");
        let health_check = created[0]
            .health_check
            .as_ref()
            .expect("health_check is set");
        assert_eq!(health_check.test[0], "CMD-SHELL");
        assert!(health_check.test[1].contains("pg_isready"));
        assert!(health_check.test[1].contains("app_salmon"));
    }

    #[tokio::test]
    async fn spawn_fails_fast_when_container_exits_during_health_check() {
        let runtime = FakeContainerRuntime {
            inspect_status: Mutex::new(Some(ContainerStatus::Exited { exit_code: 1 })),
            ..Default::default()
        };
        let backend = backend(runtime);
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let service = ServiceSpec {
            kind: ServiceKind::Postgres,
            pgvector: false,
        };
        let err = backend
            .spawn(&ClusterId::new(ulid::Ulid::nil()), &worker, 0, &service)
            .await
            .expect_err("exited container is not healthy");
        assert!(matches!(
            err,
            crate::domain::cluster::ClusterError::Docker(DockerError::ContainerNotHealthy {
                exit_code: Some(1),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn spawn_fails_fast_when_container_vanishes_during_health_check() {
        let runtime = FakeContainerRuntime {
            inspect_status: Mutex::new(Some(ContainerStatus::NotFound)),
            ..Default::default()
        };
        let backend = backend(runtime);
        let worker = WorkerUser::new("salmon-worker-00", 2000, 2000);
        let service = ServiceSpec {
            kind: ServiceKind::Postgres,
            pgvector: false,
        };
        let err = backend
            .spawn(&ClusterId::new(ulid::Ulid::nil()), &worker, 0, &service)
            .await
            .expect_err("vanished container is not healthy");
        assert!(matches!(
            err,
            crate::domain::cluster::ClusterError::Docker(DockerError::ContainerNotHealthy {
                exit_code: None,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn spawn_builds_container_spec_with_worker_ownership_and_bind_mount() {
        // The container is made to fail its health check immediately (exited) so the test can
        // inspect the `ContainerSpec` that was submitted without needing a real Postgres.
        let runtime = std::sync::Arc::new(FakeContainerRuntime {
            inspect_status: Mutex::new(Some(ContainerStatus::Exited { exit_code: 0 })),
            ..Default::default()
        });
        let backend = PostgresBackend::new(
            runtime.clone() as std::sync::Arc<dyn ContainerRuntime>,
            std::sync::Arc::new(FixedSecretGenerator),
            "pgvector/pgvector:pg16".to_string(),
            std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            Duration::from_millis(50),
        );

        let cluster_id = ClusterId::new(ulid::Ulid::nil());
        let worker = WorkerUser::new("salmon-worker-05", 2005, 2005);
        let service = ServiceSpec {
            kind: ServiceKind::Postgres,
            pgvector: true,
        };
        let _ = backend.spawn(&cluster_id, &worker, 3, &service).await;

        let created = runtime.created.lock().expect("lock");
        assert_eq!(created.len(), 1);
        let spec = &created[0];
        assert_eq!(spec.name, container_name(&cluster_id));
        assert_eq!(spec.run_as, Some((2005, 2005)));
        assert_eq!(
            spec.bind_mount.as_ref().expect("bind mount set").host_path,
            "/var/lib/app_salmon/workers/salmon-worker-05/slot-3"
        );
        assert!(spec.env.iter().any(|(key, _)| key == "POSTGRES_PASSWORD"));
        assert_eq!(
            spec.labels.get("app_salmon.cluster_id"),
            Some(&cluster_id.to_string())
        );
    }

    #[tokio::test]
    async fn teardown_calls_stop_and_remove_with_deterministic_handle() {
        let runtime = std::sync::Arc::new(FakeContainerRuntime::default());
        let backend = PostgresBackend::new(
            runtime.clone() as std::sync::Arc<dyn ContainerRuntime>,
            std::sync::Arc::new(FixedSecretGenerator),
            "pgvector/pgvector:pg16".to_string(),
            std::path::PathBuf::from("/var/lib/app_salmon/workers"),
            Duration::from_millis(50),
        );
        let cluster_id = ClusterId::new(ulid::Ulid::nil());
        backend
            .teardown(&cluster_id)
            .await
            .expect("teardown succeeds");

        let removed = runtime.removed.lock().expect("lock");
        assert_eq!(removed.len(), 1);
        assert_eq!(
            removed[0],
            ContainerHandle::new(container_name(&cluster_id))
        );
    }

    #[test]
    fn secret_generator_fake_produces_requested_length() {
        let generator = FixedSecretGenerator;
        assert_eq!(generator.db_password(10).len(), 10);
    }

    #[tokio::test]
    async fn is_alive_true_when_container_is_running() {
        let runtime = FakeContainerRuntime {
            inspect_status: Mutex::new(Some(ContainerStatus::Running {
                published_port: Some(5432),
                health: Some(crate::ports::container_runtime::HealthState::Healthy),
            })),
            ..Default::default()
        };
        let backend = backend(runtime);
        let cluster_id = ClusterId::new(ulid::Ulid::nil());
        assert!(
            backend
                .is_alive(&cluster_id)
                .await
                .expect("inspect succeeds")
        );
    }

    #[tokio::test]
    async fn is_alive_false_when_container_exited() {
        let runtime = FakeContainerRuntime {
            inspect_status: Mutex::new(Some(ContainerStatus::Exited { exit_code: 0 })),
            ..Default::default()
        };
        let backend = backend(runtime);
        let cluster_id = ClusterId::new(ulid::Ulid::nil());
        assert!(
            !backend
                .is_alive(&cluster_id)
                .await
                .expect("inspect succeeds")
        );
    }

    #[tokio::test]
    async fn is_alive_false_when_container_not_found() {
        let runtime = FakeContainerRuntime {
            inspect_status: Mutex::new(Some(ContainerStatus::NotFound)),
            ..Default::default()
        };
        let backend = backend(runtime);
        let cluster_id = ClusterId::new(ulid::Ulid::nil());
        assert!(
            !backend
                .is_alive(&cluster_id)
                .await
                .expect("inspect succeeds")
        );
    }
}
