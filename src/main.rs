use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use app_salmon::adapters::docker_bollard::BollardContainerRuntime;
use app_salmon::adapters::rand_secrets::RandSecretGenerator;
use app_salmon::adapters::sqlite_repository::SqliteClusterRepository;
use app_salmon::adapters::sudo_exec::SudoExecutor;
use app_salmon::adapters::system_clock::SystemClock;
use app_salmon::adapters::system_users::{self, WorkerResolutionError};
use app_salmon::backends::ClusterBackend;
use app_salmon::backends::postgres::PostgresBackend;
use app_salmon::client_workers::ClientWorkers;
use app_salmon::config::{Config, ConfigError};
use app_salmon::domain::ids::ClientId;
use app_salmon::domain::service_kind::ServiceKind;
use app_salmon::http::{self, AppState};
use app_salmon::ports::container_runtime::DockerError;
use app_salmon::ports::repository::RepositoryError;
use app_salmon::service::cluster_service::{ClusterService, Limits};
use app_salmon::service::deps::{TaskDeps, TaskRegistry};
use app_salmon::service::{log_rotation, reconciliation, ttl_reaper};
use app_salmon::telemetry::{self, TelemetryError};
use chrono::TimeDelta;
use clap::Parser;

const SYSTEM_PASSWD_PATH: &str = "/etc/passwd";
const LOG_ROTATION_INTERVAL: Duration = Duration::from_hours(1);
const LOG_COMPRESS_AFTER: Duration = Duration::from_hours(24);

/// Command-line arguments for the `app_salmon` binary.
#[derive(Parser)]
#[command(
    name = "app_salmon",
    about = "Provisions ephemeral Postgres+pgvector clusters for integration tests"
)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
}

/// Every way `app_salmon` can fail to start or fail while serving. `main` returns this as `Err`
/// rather than panicking, so a startup/runtime failure exits the process non-zero with a message
/// on stderr instead of an unwind.
#[derive(Debug, thiserror::Error)]
enum AppStartupError {
    /// The config file couldn't be read, couldn't be parsed as valid TOML matching the expected
    /// shape, or parsed but failed validation (e.g. `min_ttl_secs >= max_ttl_secs`).
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The `tracing` subscriber failed to initialize — e.g. the configured log directory
    /// couldn't be created.
    #[error(transparent)]
    Telemetry(#[from] TelemetryError),
    /// Resolving a configured client's Unix account uid/gid against `/etc/passwd` failed — the
    /// account doesn't exist, or the passwd file couldn't be read/parsed.
    #[error(transparent)]
    WorkerResolution(#[from] WorkerResolutionError),
    /// The `SQLite`-backed cluster repository couldn't be opened.
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    /// The Docker daemon socket couldn't be connected to.
    #[error(transparent)]
    Docker(#[from] DockerError),
    /// A required directory (currently only the `SQLite` database file's parent directory)
    /// couldn't be created.
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        /// The directory that failed to be created.
        path: PathBuf,
        /// The underlying I/O error from the failed `create_dir_all` call.
        #[source]
        source: std::io::Error,
    },
    /// The HTTP listener couldn't bind to the configured address, or (this variant is reused for
    /// both) the server failed while serving after a successful bind.
    #[error("failed to bind {addr}: {source}")]
    Bind {
        /// The address that failed to bind, or that the now-failed server was listening on.
        addr: SocketAddr,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Ensures `path`'s parent directory exists, creating it (and any missing ancestors) if not. A
/// no-op if `path` has no parent component.
///
/// # Arguments
///
/// - `path`: the file path whose parent directory should exist; `path` itself is not created or
///   inspected.
///
/// # Errors
///
/// Returns [`AppStartupError::CreateDir`] if the directory couldn't be created — e.g. a
/// permissions problem, or an existing path component that isn't a directory.
async fn ensure_parent_dir(path: &Path) -> Result<(), AppStartupError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| AppStartupError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

/// Builds every real adapter, then runs startup reconciliation (must complete before the listener
/// binds, so `list`/`info` never contradict real backend state right after a restart).
///
/// # Arguments
///
/// - `config`: the loaded, validated application configuration — supplies each client's Unix
///   account, the Docker socket path and Postgres image, and the health-check timeout.
/// - `repository`: the shared cluster repository. Passed in (not constructed here) so this
///   function and its caller operate on the exact same handle rather than two separate opens of
///   the same database file.
/// - `clock`: the shared clock, likewise passed in rather than constructed here.
/// - `secrets`: the shared secret generator, handed to the Postgres backend for generating DB
///   passwords.
///
/// # Returns
///
/// The fully assembled [`TaskDeps`].
///
/// # Errors
///
/// Returns [`AppStartupError::WorkerResolution`] if resolving any configured client's Unix
/// account uid/gid against `/etc/passwd` fails, or [`AppStartupError::Docker`] if the Docker
/// daemon socket can't be connected to. Startup reconciliation itself never fails this function —
/// it logs and continues past any repository or backend error it encounters internally.
async fn build_task_deps(
    config: &Config,
    repository: Arc<SqliteClusterRepository>,
    clock: Arc<SystemClock>,
    secrets: Arc<RandSecretGenerator>,
) -> Result<Arc<TaskDeps>, AppStartupError> {
    let configured_clients: Vec<(ClientId, String)> = config
        .clients
        .iter()
        .map(|client| (ClientId::new(client.name.clone()), client.unix_user.clone()))
        .collect();
    let client_workers =
        system_users::resolve_client_workers(Path::new(SYSTEM_PASSWD_PATH), &configured_clients)
            .await?;
    tracing::info!(
        clients = client_workers.len(),
        "resolved configured clients' unix accounts"
    );

    let container_runtime = Arc::new(BollardContainerRuntime::connect(
        &config.docker.socket_path,
        10,
    )?);
    let privileged_exec = Arc::new(SudoExecutor::new("sudo", Duration::from_secs(30)));
    let worker_data_dir_base = config
        .storage
        .sqlite_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("workers");

    let postgres_backend = Arc::new(PostgresBackend::new(
        container_runtime,
        secrets.clone(),
        config.docker.postgres_image.clone(),
        worker_data_dir_base.clone(),
        Duration::from_secs(config.limits.health_check_timeout_secs),
    ));
    let mut backends: HashMap<ServiceKind, Arc<dyn ClusterBackend>> = HashMap::new();
    backends.insert(ServiceKind::Postgres, postgres_backend);

    let task_deps = Arc::new(TaskDeps {
        repository: repository.clone(),
        client_workers: Arc::new(ClientWorkers::new(client_workers)),
        privileged_exec,
        backends,
        clock: clock.clone(),
        worker_data_dir_base,
    });

    reconciliation::run(&task_deps).await;
    tracing::info!("startup reconciliation complete");

    Ok(task_deps)
}

/// Runs the whole service end to end: initializes telemetry, opens the repository, builds every
/// adapter (via [`build_task_deps`], including startup reconciliation), spawns the background
/// tasks (TTL reaper, log rotation), binds the HTTP listener, and serves requests until a shutdown
/// signal arrives.
///
/// # Arguments
///
/// - `config`: the loaded, validated application configuration for this run.
///
/// # Returns
///
/// `Ok(())` once the server has shut down gracefully — a shutdown signal (Ctrl-C or `SIGTERM`)
/// was received and [`shutdown_signal`]'s future resolved, allowing `axum::serve` to stop
/// accepting new connections and finish in-flight ones.
///
/// # Errors
///
/// Returns [`AppStartupError`] if any startup step fails — creating the `SQLite` parent
/// directory, initializing telemetry, opening the repository, building adapters (via
/// [`build_task_deps`]), parsing client credentials, or binding the listener — or if the HTTP
/// server itself fails while serving.
async fn run(config: Config) -> Result<(), AppStartupError> {
    ensure_parent_dir(&config.storage.sqlite_path).await?;
    let _telemetry_guard = telemetry::init(&config.logging.log_dir)?;

    tracing::info!(bind_addr = %config.server.bind_addr, "starting app_salmon");

    let repository = Arc::new(SqliteClusterRepository::open(&config.storage.sqlite_path).await?);
    let clock = Arc::new(SystemClock);
    let secrets = Arc::new(RandSecretGenerator);

    let task_deps =
        build_task_deps(&config, repository.clone(), clock.clone(), secrets.clone()).await?;

    let cluster_service = Arc::new(ClusterService::new(
        repository.clone(),
        clock.clone(),
        secrets.clone(),
        Limits {
            min_ttl: TimeDelta::seconds(config.limits.min_ttl_secs),
            max_ttl: TimeDelta::seconds(config.limits.max_ttl_secs),
            max_clusters_per_user: config.limits.max_clusters_per_user,
        },
    ));
    let client_registry = Arc::new(config.client_registry().map_err(AppStartupError::Config)?);

    let state = AppState {
        cluster_service: cluster_service.clone(),
        client_registry,
        task_deps: task_deps.clone(),
        task_registry: Arc::new(TaskRegistry::new()),
        spawn_estimate: TimeDelta::seconds(
            i64::try_from(config.limits.spawn_estimate_secs).unwrap_or(i64::MAX),
        ),
    };

    tokio::spawn(ttl_reaper::run_forever(
        cluster_service,
        task_deps,
        Duration::from_secs(config.limits.ttl_reaper_interval_secs),
        TimeDelta::seconds(
            i64::try_from(config.limits.failed_cluster_reap_delay_secs).unwrap_or(i64::MAX),
        ),
    ));
    tokio::spawn(log_rotation::run_forever(
        app_salmon::adapters::tokio_filesystem::TokioFilesystem,
        config.logging.log_dir.clone(),
        LOG_ROTATION_INTERVAL,
        LOG_COMPRESS_AFTER,
        Duration::from_secs(u64::from(config.logging.retention_days) * 86_400),
    ));

    let listener = tokio::net::TcpListener::bind(config.server.bind_addr)
        .await
        .map_err(|source| AppStartupError::Bind {
            addr: config.server.bind_addr,
            source,
        })?;
    tracing::info!(addr = %config.server.bind_addr, "listening");

    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|source| AppStartupError::Bind {
            addr: config.server.bind_addr,
            source,
        })?;

    Ok(())
}

/// Resolves as soon as a shutdown signal arrives — Ctrl-C, or on Unix, `SIGTERM` — whichever
/// comes first. Passed to `axum::serve`'s `with_graceful_shutdown` so the server stops accepting
/// new connections and finishes in-flight ones instead of being killed mid-request.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutdown signal received");
}

/// Binary entry point: parses CLI arguments, loads the config file they point to, and runs the
/// service. Every failure at every stage is returned as `Err` rather than panicking, so
/// `#[tokio::main]`'s default error handling prints [`AppStartupError`]'s `Display` text to
/// stderr and exits non-zero, instead of an unwind.
///
/// # Returns
///
/// `Ok(())` on a clean shutdown.
///
/// # Errors
///
/// Returns [`AppStartupError::Config`] if the config file named on the command line can't be
/// loaded, or propagates whatever [`run`] itself fails with.
#[tokio::main]
async fn main() -> Result<(), AppStartupError> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config).await?;
    run(config).await
}
