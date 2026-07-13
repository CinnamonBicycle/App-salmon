//! Port for driving container lifecycle. The real adapter (`adapters::docker_bollard`) talks to
//! a Docker Engine API socket via `bollard`; unit tests use `FakeContainerRuntime` or point the
//! real adapter at a fake Docker Engine API server (see `adapters::docker_bollard` tests) so the
//! adapter's own code is exercised without a real daemon.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

/// A Docker `HEALTHCHECK` to run inside the container, polled via [`ContainerStatus::Running`]'s
/// `health` field instead of the backend dialing the service itself — see `docs/DESIGN.md` for why
/// `backends::postgres` moved from an app-level `tokio_postgres::connect()` readiness probe to
/// this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheck {
    /// The command to run inside the container to probe health, Docker `HEALTHCHECK` style, e.g.
    /// `["CMD-SHELL", "pg_isready -U app_salmon"]`. Exit code `0` means healthy.
    pub test: Vec<String>,
    /// How long to wait between successive health check runs.
    pub interval: Duration,
    /// How long to wait for a single health check run to complete before treating it as failed.
    pub timeout: Duration,
    /// How many consecutive failed checks before the container is considered `unhealthy`.
    pub retries: u32,
}

/// Mirrors Docker's own `none`/`starting`/`healthy`/`unhealthy` health states — a container's own
/// type, not `bollard`'s, so this port stays independent of the adapter underneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    /// The container has no `HEALTHCHECK` configured, so no health status is available.
    None,
    /// A `HEALTHCHECK` is configured but hasn't reported a definitive result yet (e.g. still
    /// within its initial grace period, or its first run hasn't completed).
    Starting,
    /// The most recent health check(s) succeeded.
    Healthy,
    /// The health check has failed `retries` consecutive times.
    Unhealthy,
}

/// Opaque handle to a container the runtime created. Carries just enough to look the container
/// back up later (inspect/stop/remove) — never anything secret.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContainerHandle(
    /// The container's name or daemon-assigned id, whichever the runtime uses to address it.
    String,
);

impl ContainerHandle {
    /// Wraps a raw container name/id as a [`ContainerHandle`].
    ///
    /// # Arguments
    ///
    /// - `container_id`: the container's name or daemon-assigned id.
    ///
    /// # Returns
    ///
    /// The wrapped handle.
    #[must_use]
    pub fn new(container_id: impl Into<String>) -> Self {
        Self(container_id.into())
    }

    /// Borrows the underlying container name/id as a plain string slice.
    ///
    /// # Returns
    ///
    /// The raw container name/id this handle wraps.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContainerHandle {
    /// Writes the underlying container name/id, unmodified.
    ///
    /// # Arguments
    ///
    /// - `f`: the formatter to write the container name/id to.
    ///
    /// # Returns
    ///
    /// `Ok(())` on success, or the formatter's write error.
    ///
    /// # Errors
    ///
    /// Returns an error only if writing to `f` itself fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A single `host_path:container_path` bind mount, owned on the host side by a specific worker
/// uid/gid so the container's on-disk state is attributable to one worker account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindMount {
    /// Absolute path on the host to mount into the container.
    pub host_path: String,
    /// Absolute path inside the container where `host_path` is mounted.
    pub container_path: String,
}

/// The OCI/containerd runtime a container is created under — a closed enum, not a free-form
/// string, so every call site states its choice explicitly and the compiler catches an
/// unhandled variant if a third one is ever added. Named `OciRuntime` rather than `ContainerRuntime`
/// specifically to avoid colliding with this port's own trait name.
///
/// [`OciRuntime::Kata`] is **not** "Docker running inside a VM" — it's a drop-in replacement for
/// `runc` that boots a minimal micro-VM per container and runs the container's process directly
/// inside it, with no nested container runtime in the guest. From this port's perspective the two
/// variants differ only in which runtime string reaches the daemon; see `docs/DESIGN.md` §11 for
/// how this was verified (a real, separately-kerneled micro-VM per `Kata` container, confirmed on
/// a real KVM host) and exactly what installing/registering it involves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OciRuntime {
    /// The daemon's default OCI runtime — shared-kernel namespace/cgroup isolation. Used for
    /// trusted, vetted software (e.g. Postgres, `PostgREST`, `GoTrue`, Kong).
    Runc,
    /// Kata Containers — per-container micro-VM isolation. Used for arbitrary, untrusted
    /// caller-supplied code (e.g. Supabase edge functions).
    Kata,
}

/// Opaque handle to a Docker network the runtime created. Carries just enough to look the network
/// back up later (remove), mirroring [`ContainerHandle`]'s shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NetworkHandle(
    /// The network's name — chosen by us (deterministically derived from a `ClusterId`), not
    /// daemon-assigned, so nothing extra needs to be persisted for later lookup.
    String,
);

impl NetworkHandle {
    /// Wraps a raw network name as a [`NetworkHandle`].
    ///
    /// # Arguments
    ///
    /// - `name`: the network's name.
    ///
    /// # Returns
    ///
    /// The wrapped handle.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Borrows the underlying network name as a plain string slice.
    ///
    /// # Returns
    ///
    /// The raw network name this handle wraps.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NetworkHandle {
    /// Writes the underlying network name, unmodified.
    ///
    /// # Arguments
    ///
    /// - `f`: the formatter to write the network name to.
    ///
    /// # Returns
    ///
    /// `Ok(())` on success, or the formatter's write error.
    ///
    /// # Errors
    ///
    /// Returns an error only if writing to `f` itself fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Attaches a container to a Docker network under a DNS-resolvable name other containers on that
/// network can reach it by (e.g. Kong reaching Postgres via the alias `db`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkAttachment {
    /// The network to join — must already exist (created via
    /// [`ContainerRuntime::create_network`]).
    pub network_name: String,
    /// The DNS name other containers on the same network reach this one by.
    pub alias: String,
}

/// Declarative description of the container to create. Intentionally has no notion of "how to
/// get here" (no builder mutation) — a `ContainerSpec` is a value, constructed once by a backend
/// (e.g. `backends::postgres`) and handed to the runtime.
///
/// Does not derive `Debug` from `#[derive]`: `env` routinely carries a freshly generated DB
/// password (e.g. `POSTGRES_PASSWORD`), so a blanket derive would make `{:?}` on this type a
/// live secret-leak footgun for whoever adds a debug log line later. The hand-written impl below
/// shows env var *names* only.
#[derive(Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    /// The name to create the container under — also used as its [`ContainerHandle`], since a
    /// name chosen by us (rather than a daemon-assigned id) can be recomputed deterministically
    /// from a `ClusterId` alone, with nothing extra to persist for later lookup.
    pub name: String,
    /// The image reference to create the container from (e.g. `pgvector/pgvector:pg16`).
    pub image: String,
    /// Environment variables to set inside the container, as `(name, value)` pairs.
    pub env: Vec<(String, String)>,
    /// Labels to attach to the container, used to tag it with identifying metadata (e.g. the
    /// owning cluster id) without needing a separate lookup table.
    pub labels: HashMap<String, String>,
    /// Host port to publish `container_port` on. `None` lets the daemon pick an ephemeral port.
    pub host_port: Option<u16>,
    /// The port inside the container to publish on the host.
    pub container_port: u16,
    /// The single bind mount to attach, if any.
    pub bind_mount: Option<BindMount>,
    /// `--user uid:gid`, if set; ties the in-container process to a specific worker account.
    pub run_as: Option<(u32, u32)>,
    /// If set, overrides the image's own `HEALTHCHECK` (or adds one to images that don't declare
    /// one). Readiness polling (`ContainerStatus::Running`'s `health` field) only reflects
    /// anything meaningful if either this or the image itself declares a healthcheck.
    pub health_check: Option<HealthCheck>,
    /// Which OCI runtime creates this container. Non-optional and explicit — no implicit
    /// "unspecified means whatever the daemon defaults to" — every call site states its choice.
    pub runtime: OciRuntime,
    /// Which Docker network to join and under what alias, if any. `None` means the container
    /// gets only the daemon's default network with no custom alias.
    pub network: Option<NetworkAttachment>,
}

impl fmt::Debug for ContainerSpec {
    /// Formats every field except `env`, whose values are replaced with just their key names —
    /// see the type-level doc comment for why: `env` routinely carries a live secret, and a
    /// blanket derive would silently print it if anyone added a `{:?}` debug log line later.
    ///
    /// # Arguments
    ///
    /// - `f`: the formatter to write the struct's field list to.
    ///
    /// # Returns
    ///
    /// `Ok(())` on success, or the formatter's write error.
    ///
    /// # Errors
    ///
    /// Returns an error only if writing to `f` itself fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env_names: Vec<&str> = self.env.iter().map(|(key, _value)| key.as_str()).collect();
        f.debug_struct("ContainerSpec")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("env_names", &env_names)
            .field("labels", &self.labels)
            .field("host_port", &self.host_port)
            .field("container_port", &self.container_port)
            .field("bind_mount", &self.bind_mount)
            .field("run_as", &self.run_as)
            .field("health_check", &self.health_check)
            .field("runtime", &self.runtime)
            .field("network", &self.network)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerStatus {
    /// The container exists and its process is running.
    Running {
        /// The host port the daemon bound to `container_port`, once it's been assigned — `None`
        /// briefly after start if it was requested with `host_port: None` and hasn't been
        /// reported back yet.
        published_port: Option<u16>,
        /// The container's current `HEALTHCHECK` status. `None` if the container has no
        /// healthcheck configured (neither [`ContainerSpec::health_check`] nor one baked into the
        /// image).
        health: Option<HealthState>,
    },
    /// The container exists but its process has stopped.
    Exited {
        /// The process's exit code.
        exit_code: i32,
    },
    /// No container with the looked-up handle exists (never created, or already removed).
    NotFound,
}

#[derive(Debug, Error)]
pub enum DockerError {
    /// Couldn't reach the Docker daemon at all.
    #[error("failed to connect to docker daemon at {socket}: {source}")]
    Connect {
        /// The socket path that was dialed.
        socket: String,
        /// The underlying `bollard`/transport error.
        #[source]
        source: bollard::errors::Error,
    },
    /// The daemon rejected (or failed to process) a container-create request.
    #[error("failed to create container: {source}")]
    CreateContainer {
        /// The underlying `bollard` error.
        #[source]
        source: bollard::errors::Error,
    },
    /// The daemon rejected (or failed to process) a container-start request.
    #[error("failed to start container {container}: {source}")]
    StartContainer {
        /// The container that failed to start.
        container: ContainerHandle,
        /// The underlying `bollard` error.
        #[source]
        source: bollard::errors::Error,
    },
    /// A request to inspect a container's current state failed (other than the container simply
    /// not existing, which is [`ContainerStatus::NotFound`], not an error).
    #[error("failed to inspect container {container}: {source}")]
    InspectContainer {
        /// The container that failed to be inspected.
        container: ContainerHandle,
        /// The underlying `bollard` error.
        #[source]
        source: bollard::errors::Error,
    },
    /// A request to stop and/or remove a container failed (other than the container already
    /// being gone, which is treated as success, not an error).
    #[error("failed to stop/remove container {container}: {source}")]
    RemoveContainer {
        /// The container that failed to be stopped/removed.
        container: ContainerHandle,
        /// The underlying `bollard` error.
        #[source]
        source: bollard::errors::Error,
    },
    /// A caller polling for readiness gave up after waiting the configured amount of time
    /// without the container reporting healthy.
    #[error("container {container} did not become healthy within {waited_secs}s")]
    HealthCheckTimeout {
        /// The container that never became healthy.
        container: ContainerHandle,
        /// How long the caller waited before giving up, in seconds.
        waited_secs: u64,
    },
    /// The container exited or vanished while a backend was still waiting for it to become
    /// ready — distinct from [`DockerError::HealthCheckTimeout`], which means it never crashed,
    /// just never became reachable in time.
    #[error("container {container} was not healthy (exit code {exit_code:?})")]
    ContainerNotHealthy {
        /// The container that exited or vanished.
        container: ContainerHandle,
        /// The process's exit code, if it exited (as opposed to vanishing entirely).
        exit_code: Option<i32>,
    },
    /// The daemon rejected (or failed to process) a network-create request.
    #[error("failed to create network {name}: {source}")]
    CreateNetwork {
        /// The network name that failed to be created.
        name: String,
        /// The underlying `bollard` error.
        #[source]
        source: bollard::errors::Error,
    },
    /// A request to remove a network failed (other than the network already being gone, which is
    /// treated as success, not an error).
    #[error("failed to remove network {network}: {source}")]
    RemoveNetwork {
        /// The network that failed to be removed.
        network: NetworkHandle,
        /// The underlying `bollard` error.
        #[source]
        source: bollard::errors::Error,
    },
}

#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    /// Creates a container from `spec` and starts it.
    ///
    /// # Arguments
    ///
    /// - `spec`: the declarative description of the container to create.
    ///
    /// # Returns
    ///
    /// A handle to the newly created and started container.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::CreateContainer`] if the daemon rejects the create request, or
    /// [`DockerError::StartContainer`] if creation succeeds but starting it fails.
    async fn create_and_start(&self, spec: &ContainerSpec) -> Result<ContainerHandle, DockerError>;

    /// Looks up a container's current state.
    ///
    /// # Arguments
    ///
    /// - `handle`: the container to inspect.
    ///
    /// # Returns
    ///
    /// The container's current [`ContainerStatus`] — including [`ContainerStatus::NotFound`] if
    /// no such container exists, which is a normal outcome, not an error.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::InspectContainer`] if the daemon can't be reached or returns an
    /// unexpected failure while inspecting the container.
    async fn inspect(&self, handle: &ContainerHandle) -> Result<ContainerStatus, DockerError>;

    /// Stops and removes a container. Idempotent: a container that's already gone is treated as
    /// success, not an error.
    ///
    /// # Arguments
    ///
    /// - `handle`: the container to stop and remove.
    ///
    /// # Returns
    ///
    /// Nothing, on success.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::RemoveContainer`] if the daemon can't be reached or returns an
    /// unexpected failure while stopping/removing the container.
    async fn stop_and_remove(&self, handle: &ContainerHandle) -> Result<(), DockerError>;

    /// Creates a Docker network for containers to be attached to via
    /// [`ContainerSpec::network`], so they can address each other by name (e.g. Kong reaching
    /// Postgres via the alias `db`).
    ///
    /// # Arguments
    ///
    /// - `name`: the network's name — chosen by the caller, deterministically, so it can be
    ///   recomputed later for removal with nothing extra to persist.
    ///
    /// # Returns
    ///
    /// A handle to the newly created network.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::CreateNetwork`] if the daemon rejects the create request.
    async fn create_network(&self, name: &str) -> Result<NetworkHandle, DockerError>;

    /// Removes a network. Idempotent: a network that's already gone is treated as success, not
    /// an error. Callers must remove every container attached to a network before removing the
    /// network itself — the daemon refuses to remove a network with containers still attached.
    ///
    /// # Arguments
    ///
    /// - `handle`: the network to remove.
    ///
    /// # Returns
    ///
    /// Nothing, on success.
    ///
    /// # Errors
    ///
    /// Returns [`DockerError::RemoveNetwork`] if the daemon can't be reached or returns an
    /// unexpected failure while removing the network.
    async fn remove_network(&self, handle: &NetworkHandle) -> Result<(), DockerError>;
}

#[cfg(test)]
mod tests {
    use super::{
        BindMount, ContainerHandle, ContainerSpec, NetworkAttachment, NetworkHandle, OciRuntime,
    };
    use std::collections::HashMap;

    #[test]
    fn container_handle_display_shows_the_raw_id() {
        let handle = ContainerHandle::new("app-salmon-01ABC");
        assert_eq!(handle.to_string(), "app-salmon-01ABC");
        assert_eq!(handle.as_str(), "app-salmon-01ABC");
    }

    #[test]
    fn network_handle_display_shows_the_raw_name() {
        let handle = NetworkHandle::new("app-salmon-net-01ABC");
        assert_eq!(handle.to_string(), "app-salmon-net-01ABC");
        assert_eq!(handle.as_str(), "app-salmon-net-01ABC");
    }

    fn sample_spec() -> ContainerSpec {
        let mut labels = HashMap::new();
        labels.insert("app_salmon.cluster_id".to_string(), "01ABC".to_string());
        ContainerSpec {
            name: "app-salmon-01ABC".to_string(),
            image: "pgvector/pgvector:pg16".to_string(),
            env: vec![
                ("POSTGRES_USER".to_string(), "app_salmon".to_string()),
                (
                    "POSTGRES_PASSWORD".to_string(),
                    "super-secret-value".to_string(),
                ),
            ],
            labels,
            host_port: Some(55432),
            container_port: 5432,
            bind_mount: Some(BindMount {
                host_path: "/var/lib/app_salmon/workers/salmon-worker-00".to_string(),
                container_path: "/var/lib/postgresql/data".to_string(),
            }),
            run_as: Some((2000, 2000)),
            health_check: None,
            runtime: OciRuntime::Runc,
            network: Some(NetworkAttachment {
                network_name: "app-salmon-net-01ABC".to_string(),
                alias: "db".to_string(),
            }),
        }
    }

    #[test]
    fn container_spec_debug_never_includes_env_values() {
        let debug_output = format!("{:?}", sample_spec());
        assert!(
            !debug_output.contains("super-secret-value"),
            "password leaked into Debug output: {debug_output}"
        );
        assert!(
            debug_output.contains("POSTGRES_PASSWORD"),
            "env var name should still be visible: {debug_output}"
        );
        assert!(debug_output.contains("POSTGRES_USER"));
    }

    #[test]
    fn container_spec_debug_includes_non_secret_fields() {
        let debug_output = format!("{:?}", sample_spec());
        assert!(debug_output.contains("app-salmon-01ABC"));
        assert!(debug_output.contains("pgvector/pgvector:pg16"));
        assert!(debug_output.contains("55432"));
        assert!(debug_output.contains("5432"));
        assert!(debug_output.contains("salmon-worker-00"));
        assert!(debug_output.contains("2000"));
        assert!(debug_output.contains("Runc"));
        assert!(debug_output.contains("app-salmon-net-01ABC"));
    }
}
