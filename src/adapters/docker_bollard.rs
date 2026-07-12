//! `ContainerRuntime` via `bollard`, talking to a Docker Engine API socket.
//!
//! Unit tests point this at a small fake Docker Engine API server (below, `test_support`)
//! instead of a real daemon — a `tokio::net::UnixListener` serving canned responses for the
//! handful of endpoints this adapter calls (`axum::serve` supports Unix listeners directly, so
//! the fake server is a normal small `axum::Router`). That exercises this adapter's real
//! `bollard` call sites, request construction, and response-mapping code without needing Docker
//! installed or root.
//!
//! Only the first published port mapping is read back from an inspect response — correct as
//! long as backends only ever expose one port per container, which is true for phase 1's only
//! backend (`backends::postgres`).

use std::collections::HashMap;

use async_trait::async_trait;
use bollard::Docker;
use bollard::errors::Error as BollardError;
use bollard::models::{
    ContainerCreateBody, ContainerInspectResponse, HealthConfig, HealthStatusEnum, HostConfig,
    PortBinding,
};
use bollard::query_parameters::{
    CreateContainerOptions, InspectContainerOptions, RemoveContainerOptions, StartContainerOptions,
};

use crate::ports::container_runtime::{
    ContainerHandle, ContainerRuntime, ContainerSpec, ContainerStatus, DockerError, HealthState,
};

/// [`ContainerRuntime`] backed by a real Docker Engine API connection.
pub struct BollardContainerRuntime {
    /// The connected `bollard` client every trait method issues requests through.
    client: Docker,
}

impl BollardContainerRuntime {
    /// Connects to a Docker Engine API socket, without yet issuing any request against it.
    ///
    /// # Arguments
    ///
    /// - `socket_path`: filesystem path to the Docker daemon's Unix domain socket (e.g.
    ///   `/var/run/docker.sock`).
    /// - `timeout_secs`: per-request timeout `bollard` applies to calls made through the
    ///   resulting client.
    ///
    /// # Returns
    ///
    /// A `BollardContainerRuntime` wrapping the connected `bollard::Docker` client.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::Connect`] if the socket can't be reached.
    pub fn connect(socket_path: &str, timeout_secs: u64) -> Result<Self, DockerError> {
        let client =
            Docker::connect_with_unix(socket_path, timeout_secs, bollard::API_DEFAULT_VERSION)
                .map_err(|source| DockerError::Connect {
                    socket: socket_path.to_string(),
                    source,
                })?;
        Ok(Self { client })
    }
}

/// Whether a `bollard` error represents the daemon responding `404 Not Found` — used to treat
/// "the container is already gone" as success rather than an error in `inspect`/`stop_and_remove`.
///
/// # Arguments
///
/// - `error`: the error returned by a `bollard` call.
///
/// # Returns
///
/// `true` if `error` is a `404` response from the daemon, `false` for any other error shape.
fn is_not_found(error: &BollardError) -> bool {
    matches!(
        error,
        BollardError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

/// Translates our own [`ContainerSpec`] into the `bollard`/Docker Engine API request body for
/// `POST /containers/create`.
///
/// # Arguments
///
/// - `spec`: the declarative container description to translate.
///
/// # Returns
///
/// A `ContainerCreateBody` ready to hand to `bollard::Docker::create_container` — environment
/// variables formatted as `KEY=value` strings, the bind mount (if any) as a `host:container`
/// string, the healthcheck (if any) translated into Docker's nanosecond-duration `HealthConfig`,
/// and the container's single port exposed and bound to `127.0.0.1`.
fn container_create_body(spec: &ContainerSpec) -> ContainerCreateBody {
    let env: Vec<String> = spec
        .env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    let container_port_proto = format!("{}/tcp", spec.container_port);

    let mut port_bindings = HashMap::new();
    port_bindings.insert(
        container_port_proto.clone(),
        Some(vec![PortBinding {
            host_ip: Some("127.0.0.1".to_string()),
            host_port: spec.host_port.map(|port| port.to_string()),
        }]),
    );

    let binds = spec.bind_mount.as_ref().map(|bind_mount| {
        vec![format!(
            "{}:{}",
            bind_mount.host_path, bind_mount.container_path
        )]
    });

    let healthcheck = spec.health_check.as_ref().map(|health_check| HealthConfig {
        test: Some(health_check.test.clone()),
        interval: Some(nanos(health_check.interval)),
        timeout: Some(nanos(health_check.timeout)),
        retries: Some(i64::from(health_check.retries)),
        start_period: None,
        start_interval: None,
    });

    ContainerCreateBody {
        image: Some(spec.image.clone()),
        env: (!env.is_empty()).then_some(env),
        labels: (!spec.labels.is_empty()).then(|| spec.labels.clone()),
        exposed_ports: Some(vec![container_port_proto]),
        user: spec.run_as.map(|(uid, gid)| format!("{uid}:{gid}")),
        healthcheck,
        host_config: Some(HostConfig {
            binds,
            port_bindings: Some(port_bindings),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Docker's `HealthConfig.interval`/`.timeout` are nanoseconds as `i64`; `Duration::as_nanos`
/// returns `u128`, so this saturates rather than panics on a `Duration` absurdly large enough to
/// overflow `i64` nanoseconds (over 292 years) — never expected in practice, but `as_nanos()`
/// itself would be the alternative and silently truncates instead of saturating.
///
/// # Arguments
///
/// - `duration`: the interval/timeout to convert.
///
/// # Returns
///
/// `duration`'s length in nanoseconds, saturated to `i64::MAX` if it would otherwise overflow.
fn nanos(duration: std::time::Duration) -> i64 {
    i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
}

/// Maps `bollard`'s health-status enum onto this crate's own [`HealthState`], keeping the
/// `ports::container_runtime` port independent of the `bollard` types underneath it.
///
/// # Arguments
///
/// - `status`: the health status reported by the Docker Engine API.
///
/// # Returns
///
/// The equivalent [`HealthState`] variant. `HealthStatusEnum::EMPTY` (an unset/unrecognized value)
/// is treated the same as `HealthStatusEnum::NONE`.
fn health_state_from(status: HealthStatusEnum) -> HealthState {
    match status {
        HealthStatusEnum::EMPTY | HealthStatusEnum::NONE => HealthState::None,
        HealthStatusEnum::STARTING => HealthState::Starting,
        HealthStatusEnum::HEALTHY => HealthState::Healthy,
        HealthStatusEnum::UNHEALTHY => HealthState::Unhealthy,
    }
}

/// Translates a `bollard` container-inspect response into our own [`ContainerStatus`].
///
/// # Arguments
///
/// - `response`: the raw inspect response from the Docker Engine API.
///
/// # Returns
///
/// [`ContainerStatus::Running`] (with the first published port mapping and health status, if
/// any) if the container is currently running, otherwise [`ContainerStatus::Exited`] with its
/// exit code (defaulting to `-1` if the daemon didn't report one). Never returns
/// [`ContainerStatus::NotFound`] — that's produced by the caller when the daemon itself responds
/// `404`, since a successful inspect response always describes a container that exists.
fn container_status_from_response(response: &ContainerInspectResponse) -> ContainerStatus {
    let state = response.state.as_ref();
    let running = state.and_then(|s| s.running).unwrap_or(false);
    if running {
        let published_port = response
            .network_settings
            .as_ref()
            .and_then(|settings| settings.ports.as_ref())
            .and_then(|ports| ports.values().flatten().flatten().next())
            .and_then(|binding| binding.host_port.as_ref())
            .and_then(|port_str| port_str.parse::<u16>().ok());
        let health = state
            .and_then(|s| s.health.as_ref())
            .and_then(|health| health.status)
            .map(health_state_from);
        ContainerStatus::Running {
            published_port,
            health,
        }
    } else {
        let exit_code = state
            .and_then(|s| s.exit_code)
            .and_then(|code| i32::try_from(code).ok())
            .unwrap_or(-1);
        ContainerStatus::Exited { exit_code }
    }
}

#[async_trait]
impl ContainerRuntime for BollardContainerRuntime {
    /// Creates and starts a container per `spec`, via `POST /containers/create` followed by
    /// `POST /containers/{name}/start`.
    ///
    /// # Arguments
    ///
    /// - `spec`: the declarative container description — see [`container_create_body`] for how
    ///   it's translated into the Docker Engine API request.
    ///
    /// # Returns
    ///
    /// A [`ContainerHandle`] naming the created container — always `spec.name`, since we choose
    /// container names ourselves rather than letting the daemon assign one.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::CreateContainer`] if the create request fails, or
    /// [`DockerError::StartContainer`] if the container was created but failed to start.
    async fn create_and_start(&self, spec: &ContainerSpec) -> Result<ContainerHandle, DockerError> {
        let options = CreateContainerOptions {
            name: Some(spec.name.clone()),
            platform: String::new(),
        };
        self.client
            .create_container(Some(options), container_create_body(spec))
            .await
            .map_err(|source| DockerError::CreateContainer { source })?;

        let handle = ContainerHandle::new(spec.name.clone());
        self.client
            .start_container(&spec.name, None::<StartContainerOptions>)
            .await
            .map_err(|source| DockerError::StartContainer {
                container: handle.clone(),
                source,
            })?;

        Ok(handle)
    }

    /// Looks up a container's current status via `GET /containers/{name}/json`.
    ///
    /// # Arguments
    ///
    /// - `handle`: the container to inspect.
    ///
    /// # Returns
    ///
    /// The container's current [`ContainerStatus`] — [`ContainerStatus::NotFound`] if the daemon
    /// reports the container doesn't exist (not treated as an error), otherwise
    /// [`ContainerStatus::Running`] or [`ContainerStatus::Exited`] per
    /// [`container_status_from_response`].
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::InspectContainer`] for any daemon error other than "not found".
    async fn inspect(&self, handle: &ContainerHandle) -> Result<ContainerStatus, DockerError> {
        match self
            .client
            .inspect_container(handle.as_str(), None::<InspectContainerOptions>)
            .await
        {
            Ok(response) => Ok(container_status_from_response(&response)),
            Err(source) if is_not_found(&source) => Ok(ContainerStatus::NotFound),
            Err(source) => Err(DockerError::InspectContainer {
                container: handle.clone(),
                source,
            }),
        }
    }

    /// Force-stops and removes a container (with its anonymous volumes) via
    /// `DELETE /containers/{name}`.
    ///
    /// # Arguments
    ///
    /// - `handle`: the container to stop and remove.
    ///
    /// # Returns
    ///
    /// Nothing on success.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::RemoveContainer`] for any daemon error other than "not found" — a
    /// container that's already gone is treated as a successful removal (idempotent), not an
    /// error.
    async fn stop_and_remove(&self, handle: &ContainerHandle) -> Result<(), DockerError> {
        let options = RemoveContainerOptions {
            force: true,
            v: true,
            link: false,
        };
        match self
            .client
            .remove_container(handle.as_str(), Some(options))
            .await
        {
            Ok(()) => Ok(()),
            Err(source) if is_not_found(&source) => Ok(()),
            Err(source) => Err(DockerError::RemoveContainer {
                container: handle.clone(),
                source,
            }),
        }
    }
}

#[cfg(test)]
mod test_support {
    //! A fake Docker Engine API server: just enough of the REST surface for
    //! `BollardContainerRuntime` to talk to over a real Unix socket, with test-controlled
    //! responses. Not a mock of our own trait — a stand-in for the *external* daemon, so the
    //! adapter's real `bollard` call sites actually execute.

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::{delete, get, post};
    use axum::{Json, Router};
    use serde_json::json;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum FakeHealth {
        None,
        Starting,
        Healthy,
        Unhealthy,
    }

    #[derive(Clone)]
    pub enum InspectScenario {
        Running {
            host_port: Option<u16>,
            health: FakeHealth,
        },
        Exited {
            exit_code: i32,
        },
        NotFound,
    }

    struct FakeState {
        inspect_scenario: Mutex<InspectScenario>,
        fail_create: AtomicBool,
        fail_start: AtomicBool,
        remove_not_found: AtomicBool,
        remove_server_error: AtomicBool,
        last_create_body: Mutex<Option<serde_json::Value>>,
    }

    pub struct FakeDockerEngine {
        pub socket_path: PathBuf,
        state: Arc<FakeState>,
        server_task: tokio::task::JoinHandle<()>,
        _tempdir: tempfile::TempDir,
    }

    impl Drop for FakeDockerEngine {
        fn drop(&mut self) {
            self.server_task.abort();
        }
    }

    impl FakeDockerEngine {
        pub fn start(initial_scenario: InspectScenario) -> Self {
            let tempdir = tempfile::tempdir().expect("tempdir");
            let socket_path = tempdir.path().join("docker.sock");

            let state = Arc::new(FakeState {
                inspect_scenario: Mutex::new(initial_scenario),
                fail_create: AtomicBool::new(false),
                fail_start: AtomicBool::new(false),
                remove_not_found: AtomicBool::new(false),
                remove_server_error: AtomicBool::new(false),
                last_create_body: Mutex::new(None),
            });

            let router = Router::new()
                .route("/containers/create", post(create_handler))
                .route("/containers/{name}/start", post(start_handler))
                .route("/containers/{name}/json", get(inspect_handler))
                .route("/containers/{name}", delete(remove_handler))
                .with_state(state.clone());

            let listener =
                tokio::net::UnixListener::bind(&socket_path).expect("bind fake docker socket");
            let server_task = tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });

            Self {
                socket_path,
                state,
                server_task,
                _tempdir: tempdir,
            }
        }

        pub fn set_inspect_scenario(&self, scenario: InspectScenario) {
            *self.state.inspect_scenario.lock().expect("lock") = scenario;
        }

        pub fn fail_next_create(&self) {
            self.state.fail_create.store(true, Ordering::SeqCst);
        }

        pub fn fail_next_start(&self) {
            self.state.fail_start.store(true, Ordering::SeqCst);
        }

        pub fn remove_returns_not_found(&self) {
            self.state.remove_not_found.store(true, Ordering::SeqCst);
        }

        pub fn remove_returns_server_error(&self) {
            self.state.remove_server_error.store(true, Ordering::SeqCst);
        }

        /// The JSON body of the most recent `/containers/create` request, if any — lets a test
        /// assert on exactly what was serialized and sent, not just that the call succeeded.
        pub fn last_create_body(&self) -> Option<serde_json::Value> {
            self.state.last_create_body.lock().expect("lock").clone()
        }
    }

    async fn create_handler(
        State(state): State<Arc<FakeState>>,
        Json(body): Json<serde_json::Value>,
    ) -> Response {
        *state.last_create_body.lock().expect("lock") = Some(body);
        if state.fail_create.load(Ordering::SeqCst) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "simulated create failure"})),
            )
                .into_response();
        }
        (
            StatusCode::CREATED,
            Json(json!({"Id": "fake-container-id", "Warnings": []})),
        )
            .into_response()
    }

    async fn start_handler(
        State(state): State<Arc<FakeState>>,
        Path(_name): Path<String>,
    ) -> Response {
        if state.fail_start.load(Ordering::SeqCst) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "simulated start failure"})),
            )
                .into_response();
        }
        StatusCode::NO_CONTENT.into_response()
    }

    async fn inspect_handler(
        State(state): State<Arc<FakeState>>,
        Path(_name): Path<String>,
    ) -> Response {
        let scenario = state.inspect_scenario.lock().expect("lock").clone();
        match scenario {
            InspectScenario::Running { host_port, health } => {
                let ports = host_port.map_or_else(
                    || json!({}),
                    |port| json!({"5432/tcp": [{"HostIp": "127.0.0.1", "HostPort": port.to_string()}]}),
                );
                let health_json = match health {
                    FakeHealth::None => serde_json::Value::Null,
                    FakeHealth::Starting => json!({"Status": "starting"}),
                    FakeHealth::Healthy => json!({"Status": "healthy"}),
                    FakeHealth::Unhealthy => json!({"Status": "unhealthy"}),
                };
                (
                    StatusCode::OK,
                    Json(json!({
                        "Id": "fake-container-id",
                        "State": {"Running": true, "ExitCode": 0, "Health": health_json},
                        "NetworkSettings": {"Ports": ports},
                    })),
                )
                    .into_response()
            }
            InspectScenario::Exited { exit_code } => (
                StatusCode::OK,
                Json(json!({
                    "Id": "fake-container-id",
                    "State": {"Running": false, "ExitCode": exit_code},
                    "NetworkSettings": {"Ports": {}},
                })),
            )
                .into_response(),
            InspectScenario::NotFound => (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "no such container"})),
            )
                .into_response(),
        }
    }

    async fn remove_handler(
        State(state): State<Arc<FakeState>>,
        Path(_name): Path<String>,
    ) -> Response {
        if state.remove_not_found.load(Ordering::SeqCst) {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "no such container"})),
            )
                .into_response()
        } else if state.remove_server_error.load(Ordering::SeqCst) {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "simulated remove failure"})),
            )
                .into_response()
        } else {
            StatusCode::NO_CONTENT.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BollardContainerRuntime;
    use super::test_support::{FakeDockerEngine, FakeHealth, InspectScenario};
    use crate::ports::container_runtime::{
        BindMount, ContainerHandle, ContainerRuntime, ContainerSpec, ContainerStatus, HealthState,
    };
    use std::collections::HashMap;

    fn sample_spec(name: &str) -> ContainerSpec {
        ContainerSpec {
            name: name.to_string(),
            image: "pgvector/pgvector:pg16".to_string(),
            env: vec![("POSTGRES_PASSWORD".to_string(), "secret".to_string())],
            labels: HashMap::from([("app_salmon.cluster_id".to_string(), "01ABC".to_string())]),
            host_port: None,
            container_port: 5432,
            bind_mount: Some(BindMount {
                host_path: "/var/lib/app_salmon/workers/salmon-worker-00".to_string(),
                container_path: "/var/lib/postgresql/data".to_string(),
            }),
            run_as: Some((2000, 2000)),
            health_check: None,
        }
    }

    fn runtime(engine: &FakeDockerEngine) -> BollardContainerRuntime {
        BollardContainerRuntime::connect(&engine.socket_path.to_string_lossy(), 5)
            .expect("connect to fake engine")
    }

    #[tokio::test]
    async fn create_and_start_returns_a_handle_named_after_the_spec() {
        let engine = FakeDockerEngine::start(InspectScenario::Running {
            host_port: Some(55432),
            health: FakeHealth::None,
        });
        let runtime = runtime(&engine);

        let handle = runtime
            .create_and_start(&sample_spec("app-salmon-test-1"))
            .await
            .expect("create succeeds");
        assert_eq!(handle, ContainerHandle::new("app-salmon-test-1"));
    }

    #[tokio::test]
    async fn create_and_start_sends_the_healthcheck_when_the_spec_has_one() {
        let engine = FakeDockerEngine::start(InspectScenario::Running {
            host_port: Some(55432),
            health: FakeHealth::None,
        });
        let runtime = runtime(&engine);

        let mut spec = sample_spec("app-salmon-test-healthcheck");
        spec.health_check = Some(crate::ports::container_runtime::HealthCheck {
            test: vec![
                "CMD-SHELL".to_string(),
                "pg_isready -U app_salmon".to_string(),
            ],
            interval: std::time::Duration::from_secs(1),
            timeout: std::time::Duration::from_secs(2),
            retries: 3,
        });

        runtime
            .create_and_start(&spec)
            .await
            .expect("create succeeds");

        let body = engine
            .last_create_body()
            .expect("a create request was sent");
        let healthcheck = &body["Healthcheck"];
        assert_eq!(
            healthcheck["Test"],
            serde_json::json!(["CMD-SHELL", "pg_isready -U app_salmon"])
        );
        assert_eq!(healthcheck["Interval"], 1_000_000_000);
        assert_eq!(healthcheck["Timeout"], 2_000_000_000);
        assert_eq!(healthcheck["Retries"], 3);
    }

    #[tokio::test]
    async fn create_and_start_sends_no_healthcheck_when_the_spec_has_none() {
        let engine = FakeDockerEngine::start(InspectScenario::Running {
            host_port: Some(55432),
            health: FakeHealth::None,
        });
        let runtime = runtime(&engine);

        runtime
            .create_and_start(&sample_spec("app-salmon-test-no-healthcheck"))
            .await
            .expect("create succeeds");

        let body = engine
            .last_create_body()
            .expect("a create request was sent");
        assert!(
            body.get("Healthcheck")
                .is_none_or(serde_json::Value::is_null),
            "no healthcheck should be sent when the spec doesn't set one: {body}"
        );
    }

    #[tokio::test]
    async fn inspect_reflects_scenario_changed_after_start() {
        // Models a container transitioning from "not yet publishing a port" to "publishing" —
        // the shape `backends::postgres`'s readiness poll observes across repeated calls.
        let engine = FakeDockerEngine::start(InspectScenario::Running {
            host_port: None,
            health: FakeHealth::None,
        });
        let runtime = runtime(&engine);

        let before = runtime
            .inspect(&ContainerHandle::new("anything"))
            .await
            .expect("inspect succeeds");
        assert_eq!(
            before,
            ContainerStatus::Running {
                published_port: None,
                health: None
            }
        );

        engine.set_inspect_scenario(InspectScenario::Running {
            host_port: Some(55432),
            health: FakeHealth::None,
        });
        let after = runtime
            .inspect(&ContainerHandle::new("anything"))
            .await
            .expect("inspect succeeds");
        assert_eq!(
            after,
            ContainerStatus::Running {
                published_port: Some(55432),
                health: None
            }
        );
    }

    #[tokio::test]
    async fn create_failure_is_reported() {
        let engine = FakeDockerEngine::start(InspectScenario::Running {
            host_port: None,
            health: FakeHealth::None,
        });
        engine.fail_next_create();
        let runtime = runtime(&engine);

        let err = runtime
            .create_and_start(&sample_spec("app-salmon-test-2"))
            .await
            .expect_err("create fails");
        assert!(matches!(
            err,
            crate::ports::container_runtime::DockerError::CreateContainer { .. }
        ));
    }

    #[tokio::test]
    async fn inspect_running_with_published_port() {
        let engine = FakeDockerEngine::start(InspectScenario::Running {
            host_port: Some(55432),
            health: FakeHealth::None,
        });
        let runtime = runtime(&engine);

        let status = runtime
            .inspect(&ContainerHandle::new("anything"))
            .await
            .expect("inspect succeeds");
        assert_eq!(
            status,
            ContainerStatus::Running {
                published_port: Some(55432),
                health: None
            }
        );
    }

    #[tokio::test]
    async fn inspect_running_without_published_port_yet() {
        let engine = FakeDockerEngine::start(InspectScenario::Running {
            host_port: None,
            health: FakeHealth::None,
        });
        let runtime = runtime(&engine);

        let status = runtime
            .inspect(&ContainerHandle::new("anything"))
            .await
            .expect("inspect succeeds");
        assert_eq!(
            status,
            ContainerStatus::Running {
                published_port: None,
                health: None
            }
        );
    }

    #[tokio::test]
    async fn inspect_reports_starting_healthy_and_unhealthy_status() {
        let engine = FakeDockerEngine::start(InspectScenario::Running {
            host_port: Some(55432),
            health: FakeHealth::Starting,
        });
        let runtime = runtime(&engine);

        let status = runtime
            .inspect(&ContainerHandle::new("anything"))
            .await
            .expect("inspect succeeds");
        assert_eq!(
            status,
            ContainerStatus::Running {
                published_port: Some(55432),
                health: Some(HealthState::Starting)
            }
        );

        engine.set_inspect_scenario(InspectScenario::Running {
            host_port: Some(55432),
            health: FakeHealth::Healthy,
        });
        let status = runtime
            .inspect(&ContainerHandle::new("anything"))
            .await
            .expect("inspect succeeds");
        assert_eq!(
            status,
            ContainerStatus::Running {
                published_port: Some(55432),
                health: Some(HealthState::Healthy)
            }
        );

        engine.set_inspect_scenario(InspectScenario::Running {
            host_port: Some(55432),
            health: FakeHealth::Unhealthy,
        });
        let status = runtime
            .inspect(&ContainerHandle::new("anything"))
            .await
            .expect("inspect succeeds");
        assert_eq!(
            status,
            ContainerStatus::Running {
                published_port: Some(55432),
                health: Some(HealthState::Unhealthy)
            }
        );
    }

    #[tokio::test]
    async fn inspect_exited_reports_exit_code() {
        let engine = FakeDockerEngine::start(InspectScenario::Exited { exit_code: 137 });
        let runtime = runtime(&engine);

        let status = runtime
            .inspect(&ContainerHandle::new("anything"))
            .await
            .expect("inspect succeeds");
        assert_eq!(status, ContainerStatus::Exited { exit_code: 137 });
    }

    #[tokio::test]
    async fn inspect_missing_container_is_not_found_not_an_error() {
        let engine = FakeDockerEngine::start(InspectScenario::NotFound);
        let runtime = runtime(&engine);

        let status = runtime
            .inspect(&ContainerHandle::new("anything"))
            .await
            .expect("not-found is Ok");
        assert_eq!(status, ContainerStatus::NotFound);
    }

    #[tokio::test]
    async fn start_failure_is_reported() {
        let engine = FakeDockerEngine::start(InspectScenario::Running {
            host_port: None,
            health: FakeHealth::None,
        });
        engine.fail_next_start();
        let runtime = runtime(&engine);

        let err = runtime
            .create_and_start(&sample_spec("app-salmon-test-start-failure"))
            .await
            .expect_err("start fails");
        assert!(matches!(
            err,
            crate::ports::container_runtime::DockerError::StartContainer { .. }
        ));
    }

    #[tokio::test]
    async fn stop_and_remove_reports_a_genuine_server_error_rather_than_treating_it_as_not_found() {
        let engine = FakeDockerEngine::start(InspectScenario::Running {
            host_port: None,
            health: FakeHealth::None,
        });
        engine.remove_returns_server_error();
        let runtime = runtime(&engine);

        let err = runtime
            .stop_and_remove(&ContainerHandle::new("anything"))
            .await
            .expect_err("a genuine 500 must not be swallowed as success");
        assert!(matches!(
            err,
            crate::ports::container_runtime::DockerError::RemoveContainer { .. }
        ));
    }

    #[tokio::test]
    async fn stop_and_remove_succeeds() {
        let engine = FakeDockerEngine::start(InspectScenario::Running {
            host_port: None,
            health: FakeHealth::None,
        });
        let runtime = runtime(&engine);

        runtime
            .stop_and_remove(&ContainerHandle::new("anything"))
            .await
            .expect("remove succeeds");
    }

    #[tokio::test]
    async fn stop_and_remove_is_idempotent_on_already_removed_container() {
        let engine = FakeDockerEngine::start(InspectScenario::Running {
            host_port: None,
            health: FakeHealth::None,
        });
        engine.remove_returns_not_found();
        let runtime = runtime(&engine);

        runtime
            .stop_and_remove(&ContainerHandle::new("anything"))
            .await
            .expect("404 on remove is treated as success");
    }

    #[test]
    fn connect_fails_for_nonexistent_socket() {
        match BollardContainerRuntime::connect("/nonexistent/docker.sock", 1) {
            Err(crate::ports::container_runtime::DockerError::Connect { .. }) => {}
            Err(other) => panic!("expected DockerError::Connect, got {other:?}"),
            Ok(_) => panic!("expected connect to fail for a nonexistent socket"),
        }
    }
}
