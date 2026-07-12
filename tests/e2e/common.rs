use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use app_salmon::adapters::docker_bollard::BollardContainerRuntime;
use app_salmon::adapters::rand_secrets::RandSecretGenerator;
use app_salmon::adapters::sqlite_repository::SqliteClusterRepository;
use app_salmon::adapters::sudo_exec::SudoExecutor;
use app_salmon::adapters::system_clock::SystemClock;
use app_salmon::adapters::system_users;
use app_salmon::auth::ClientRegistry;
use app_salmon::auth::hashing::SecretHash;
use app_salmon::backends::ClusterBackend;
use app_salmon::backends::postgres::PostgresBackend;
use app_salmon::domain::ids::ClientId;
use app_salmon::domain::service_kind::ServiceKind;
use app_salmon::http::{AppState, router};
use app_salmon::ports::container_runtime::{ContainerHandle, ContainerRuntime};
use app_salmon::service::cluster_service::{ClusterService, Limits};
use app_salmon::service::deps::{TaskDeps, TaskRegistry};
use app_salmon::service::{reconciliation, ttl_reaper};
use app_salmon::worker_pool::WorkerPool;
use chrono::TimeDelta;

const WORKER_PREFIX: &str = "salmon-worker-";
const WORKER_COUNT: usize = 4;
const POSTGRES_IMAGE: &str = "pgvector/pgvector:pg16";
const DOCKER_SOCKET: &str = "/var/run/docker.sock";
const WORKER_DATA_DIR_BASE: &str = "/var/lib/app_salmon/workers";
pub const CLIENT_NAME: &str = "e2e-agent";
pub const CLIENT_SECRET: &str = "e2e-secret-do-not-use-in-prod";
pub const OTHER_CLIENT_NAME: &str = "e2e-agent-other";
pub const OTHER_CLIENT_SECRET: &str = "e2e-other-secret-do-not-use-in-prod";
const REMEDIATION: &str =
    "\n\nRun `sudo ./scripts/setup-e2e-env.sh` first, then re-run `just test-e2e`.";

pub struct TestServer {
    pub base_url: String,
    pub client: reqwest::Client,
    server_task: tokio::task::JoinHandle<()>,
    reaper_task: tokio::task::JoinHandle<()>,
    _tempdir: tempfile::TempDir,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.server_task.abort();
        self.reaper_task.abort();
    }
}

/// Fails loudly (panics with a clear remediation message) if this machine isn't set up for the
/// e2e suite — never silently skips.
pub async fn ensure_prerequisites() {
    let runtime = match BollardContainerRuntime::connect(DOCKER_SOCKET, 5) {
        Ok(runtime) => runtime,
        Err(err) => panic!("docker daemon at {DOCKER_SOCKET} is not reachable: {err}{REMEDIATION}"),
    };
    // A well-formed response (even "not found") proves the daemon actually answered, unlike a
    // connection error.
    if let Err(err) = runtime
        .inspect(&ContainerHandle::new("app-salmon-e2e-prereq-check"))
        .await
    {
        panic!("docker daemon at {DOCKER_SOCKET} did not respond: {err}{REMEDIATION}");
    }

    let workers = match system_users::resolve_worker_users(
        Path::new("/etc/passwd"),
        WORKER_PREFIX,
        WORKER_COUNT,
    )
    .await
    {
        Ok(workers) => workers,
        Err(err) => panic!("worker accounts not provisioned: {err}{REMEDIATION}"),
    };
    let first_worker = workers
        .first()
        .unwrap_or_else(|| panic!("WORKER_COUNT is 0{REMEDIATION}"));

    let status = tokio::process::Command::new("sudo")
        .args(["-n", "-u", first_worker.as_str(), "true"])
        .status()
        .await
        .unwrap_or_else(|err| panic!("failed to invoke sudo: {err}{REMEDIATION}"));
    assert!(
        status.success(),
        "sudo -u {first_worker} is not permitted without a password{REMEDIATION}"
    );
}

/// Builds a fresh, fully-real `AppState` (real Docker, real sudo, real `SQLite` in a tempdir) and
/// serves it on an ephemeral local port. Each test gets its own instance — cheap, and avoids
/// tests interfering with each other's cluster rows/quota.
pub async fn spawn_test_server() -> TestServer {
    ensure_prerequisites().await;

    let tempdir = tempfile::tempdir().expect("tempdir");
    let sqlite_path = tempdir.path().join("state.sqlite3");

    let repository = Arc::new(
        SqliteClusterRepository::open(&sqlite_path)
            .await
            .expect("open sqlite"),
    );
    let container_runtime =
        Arc::new(BollardContainerRuntime::connect(DOCKER_SOCKET, 10).expect("connect docker"));
    let clock = Arc::new(SystemClock);
    let secrets = Arc::new(RandSecretGenerator);
    let privileged_exec = Arc::new(SudoExecutor::new("sudo", Duration::from_secs(30)));

    let postgres_backend = Arc::new(PostgresBackend::new(
        container_runtime,
        secrets.clone(),
        POSTGRES_IMAGE.to_string(),
        PathBuf::from(WORKER_DATA_DIR_BASE),
        Duration::from_mins(1),
    ));
    let mut backends: HashMap<ServiceKind, Arc<dyn ClusterBackend>> = HashMap::new();
    backends.insert(ServiceKind::Postgres, postgres_backend);

    let configured_workers =
        system_users::resolve_worker_users(Path::new("/etc/passwd"), WORKER_PREFIX, WORKER_COUNT)
            .await
            .expect("resolve workers");

    let task_deps = Arc::new(TaskDeps {
        repository: repository.clone(),
        worker_pool: Arc::new(WorkerPool::new(vec![])),
        privileged_exec,
        backends,
        clock: clock.clone(),
        worker_data_dir_base: PathBuf::from(WORKER_DATA_DIR_BASE),
    });
    let free_workers = reconciliation::run(&task_deps, &configured_workers).await;
    let task_deps = Arc::new(TaskDeps {
        repository: task_deps.repository.clone(),
        worker_pool: Arc::new(WorkerPool::new(free_workers)),
        privileged_exec: task_deps.privileged_exec.clone(),
        backends: task_deps.backends.clone(),
        clock: task_deps.clock.clone(),
        worker_data_dir_base: task_deps.worker_data_dir_base.clone(),
    });

    let cluster_service = Arc::new(ClusterService::new(
        repository.clone(),
        clock.clone(),
        secrets,
        Limits {
            min_ttl: TimeDelta::seconds(30),
            max_ttl: TimeDelta::seconds(3600),
            max_clusters_per_user: 2,
        },
    ));

    let mut clients = HashMap::new();
    clients.insert(ClientId::new(CLIENT_NAME), SecretHash::of(CLIENT_SECRET));
    clients.insert(
        ClientId::new(OTHER_CLIENT_NAME),
        SecretHash::of(OTHER_CLIENT_SECRET),
    );
    let client_registry = Arc::new(ClientRegistry::new(clients));

    let state = AppState {
        cluster_service: cluster_service.clone(),
        client_registry,
        task_deps: task_deps.clone(),
        task_registry: Arc::new(TaskRegistry::new()),
        spawn_estimate: TimeDelta::seconds(20),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let app = router(state);
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // A short interval so `ttl_expiry` tests (using the 30s TTL floor) don't have to wait long
    // past decommission_at for the reaper to actually act.
    let reaper_task = tokio::spawn(ttl_reaper::run_forever(
        cluster_service,
        task_deps,
        Duration::from_secs(2),
        TimeDelta::seconds(2),
    ));

    TestServer {
        base_url: format!("http://{addr}"),
        client: reqwest::Client::new(),
        server_task,
        reaper_task,
        _tempdir: tempdir,
    }
}

#[must_use]
pub fn auth_value() -> String {
    format!("Bearer {CLIENT_NAME}:{CLIENT_SECRET}")
}

#[must_use]
pub fn other_auth_value() -> String {
    format!("Bearer {OTHER_CLIENT_NAME}:{OTHER_CLIENT_SECRET}")
}

#[must_use]
pub fn unregistered_auth_value() -> String {
    "Bearer someone-unregistered:irrelevant".to_string()
}

pub async fn create_cluster(
    server: &TestServer,
    ttl_secs: i64,
) -> (reqwest::StatusCode, serde_json::Value) {
    let response = server
        .client
        .post(format!("{}/clusters", server.base_url))
        .header("authorization", auth_value())
        .json(&serde_json::json!({"service": "postgres", "pgvector": false, "ttl_secs": ttl_secs}))
        .send()
        .await
        .expect("request");
    let status = response.status();
    let body = response.json().await.expect("json");
    (status, body)
}

/// Polls `GET /clusters/{id}` until it reports `ready` or `failed`, or panics after `timeout` —
/// a real Postgres container genuinely takes real wall-clock time to become healthy.
pub async fn wait_for_ready_or_failed(
    server: &TestServer,
    id: &str,
    timeout: Duration,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let response = server
            .client
            .get(format!("{}/clusters/{id}", server.base_url))
            .header("authorization", auth_value())
            .send()
            .await
            .expect("request");
        let body: serde_json::Value = response.json().await.expect("json");
        if body["status"] == "ready" || body["status"] == "failed" {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "cluster {id} did not reach ready/failed within {timeout:?}; last body: {body}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Polls `GET /clusters/{id}` until it 404s (fully torn down — the `410 Gone` window has
/// closed), or panics after `timeout`.
pub async fn wait_for_not_found(server: &TestServer, id: &str, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let response = server
            .client
            .get(format!("{}/clusters/{id}", server.base_url))
            .header("authorization", auth_value())
            .send()
            .await
            .expect("request");
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "cluster {id} was not torn down (404) within {timeout:?}; last status: {}",
            response.status()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
