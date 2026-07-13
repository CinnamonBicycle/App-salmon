//! Shared "poll `inspect` until ready" helper, factored out of `backends::postgres`'s original
//! implementation once `backends::supabase` needed the same poll-loop shape for five containers
//! instead of one.

use std::time::Duration;

use tokio::time::Instant;

use crate::domain::cluster::ClusterError;
use crate::ports::container_runtime::{
    ContainerHandle, ContainerRuntime, ContainerStatus, DockerError, HealthState,
};

/// How often this helper asks the runtime for the current status — independent of how often
/// Docker itself runs a container's own `HEALTHCHECK` (that's `HealthCheck::interval`, set on the
/// `ContainerSpec` a backend submits).
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Polls `inspect` until `handle` is ready, or fails fast if it exits/vanishes, or times out.
///
/// "Ready" means [`ContainerStatus::Running`] with either [`HealthState::Healthy`] (the
/// container's own `HEALTHCHECK` — set via [`crate::ports::container_runtime::ContainerSpec::health_check`]
/// or baked into its image — passed) or [`HealthState::None`] (Docker's own confirmation that no
/// `HEALTHCHECK` is configured at all, a stable terminal state for a container that doesn't need
/// one — as opposed to the *absence* of a `health` field entirely, which some adapters/fakes use
/// to mean "no information yet" and which this helper keeps polling past). Any other status —
/// `Starting`, `Unhealthy`, or health information not yet available — just means "keep polling":
/// Docker itself retries a failing check before marking a container `unhealthy`, and can recover
/// it to `healthy` on a later check, so this helper's own timeout (not a single bad observation)
/// is what ultimately bounds how long a caller waits.
///
/// # Arguments
///
/// - `container_runtime`: how to inspect `handle`'s current status.
/// - `handle`: the container to poll.
/// - `timeout`: the overall deadline to wait before giving up.
///
/// # Returns
///
/// The host port the runtime published for the container, if it requested one (`None` if the
/// container was created without `host_port`/without needing external reachability at all) —
/// once it's `Running` and ready per the rules above.
///
/// # Errors
///
/// [`DockerError::ContainerNotHealthy`] (wrapped in a [`ClusterError`]) if the container exits or
/// vanishes while waiting; [`DockerError::HealthCheckTimeout`] if `timeout` elapses without the
/// container becoming ready; or whatever [`ClusterError`] `inspect` itself returns on a runtime
/// failure.
pub async fn wait_until_healthy(
    container_runtime: &dyn ContainerRuntime,
    handle: &ContainerHandle,
    timeout: Duration,
) -> Result<Option<u16>, ClusterError> {
    let deadline = Instant::now() + timeout;
    loop {
        match container_runtime.inspect(handle).await? {
            ContainerStatus::Running {
                published_port,
                health: Some(HealthState::Healthy | HealthState::None),
            } => {
                return Ok(published_port);
            }
            ContainerStatus::Running { .. } => {}
            ContainerStatus::Exited { exit_code } => {
                return Err(DockerError::ContainerNotHealthy {
                    container: handle.clone(),
                    exit_code: Some(exit_code),
                }
                .into());
            }
            ContainerStatus::NotFound => {
                return Err(DockerError::ContainerNotHealthy {
                    container: handle.clone(),
                    exit_code: None,
                }
                .into());
            }
        }
        if Instant::now() >= deadline {
            return Err(DockerError::HealthCheckTimeout {
                container: handle.clone(),
                waited_secs: timeout.as_secs(),
            }
            .into());
        }
        tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::wait_until_healthy;
    use crate::ports::container_runtime::{
        ContainerHandle, ContainerRuntime, ContainerStatus, DockerError, HealthState,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Default)]
    struct FakeRuntime {
        status: Mutex<Option<ContainerStatus>>,
    }

    #[async_trait]
    impl ContainerRuntime for FakeRuntime {
        async fn create_and_start(
            &self,
            _spec: &crate::ports::container_runtime::ContainerSpec,
        ) -> Result<ContainerHandle, DockerError> {
            unreachable!("wait_until_healthy tests never create containers")
        }

        async fn inspect(&self, _handle: &ContainerHandle) -> Result<ContainerStatus, DockerError> {
            Ok(self
                .status
                .lock()
                .expect("lock")
                .unwrap_or(ContainerStatus::Running {
                    published_port: None,
                    health: None,
                }))
        }

        async fn stop_and_remove(&self, _handle: &ContainerHandle) -> Result<(), DockerError> {
            unreachable!("wait_until_healthy tests never remove containers")
        }

        async fn create_network(
            &self,
            _name: &str,
        ) -> Result<crate::ports::container_runtime::NetworkHandle, DockerError> {
            unreachable!("wait_until_healthy tests never touch networks")
        }

        async fn remove_network(
            &self,
            _handle: &crate::ports::container_runtime::NetworkHandle,
        ) -> Result<(), DockerError> {
            unreachable!("wait_until_healthy tests never touch networks")
        }
    }

    #[tokio::test]
    async fn succeeds_with_the_published_port_once_healthy() {
        let runtime = FakeRuntime {
            status: Mutex::new(Some(ContainerStatus::Running {
                published_port: Some(55432),
                health: Some(HealthState::Healthy),
            })),
        };
        let port = wait_until_healthy(
            &runtime,
            &ContainerHandle::new("c"),
            Duration::from_millis(50),
        )
        .await
        .expect("healthy");
        assert_eq!(port, Some(55432));
    }

    #[tokio::test]
    async fn succeeds_with_no_port_when_the_container_has_no_healthcheck_configured() {
        // `HealthState::None` (Docker's own confirmation that no HEALTHCHECK is configured) is a
        // stable terminal state distinct from an entirely absent `health` field — see the
        // function's own doc comment for why this is the "no healthcheck, Running is enough" case
        // a service like PostgREST (no HEALTHCHECK set on its ContainerSpec) relies on.
        let runtime = FakeRuntime {
            status: Mutex::new(Some(ContainerStatus::Running {
                published_port: None,
                health: Some(HealthState::None),
            })),
        };
        let port = wait_until_healthy(
            &runtime,
            &ContainerHandle::new("c"),
            Duration::from_millis(50),
        )
        .await
        .expect("no-healthcheck container is ready once running");
        assert_eq!(port, None);
    }

    #[tokio::test]
    async fn times_out_when_health_information_is_never_available() {
        let runtime = FakeRuntime {
            status: Mutex::new(Some(ContainerStatus::Running {
                published_port: None,
                health: None,
            })),
        };
        let err = wait_until_healthy(
            &runtime,
            &ContainerHandle::new("c"),
            Duration::from_millis(50),
        )
        .await
        .expect_err("no health info at all never resolves as ready");
        assert!(matches!(
            err,
            crate::domain::cluster::ClusterError::Docker(DockerError::HealthCheckTimeout { .. })
        ));
    }

    #[tokio::test]
    async fn times_out_while_stuck_unhealthy() {
        let runtime = FakeRuntime {
            status: Mutex::new(Some(ContainerStatus::Running {
                published_port: Some(1),
                health: Some(HealthState::Unhealthy),
            })),
        };
        let err = wait_until_healthy(
            &runtime,
            &ContainerHandle::new("c"),
            Duration::from_millis(50),
        )
        .await
        .expect_err("stuck unhealthy never resolves as ready");
        assert!(matches!(
            err,
            crate::domain::cluster::ClusterError::Docker(DockerError::HealthCheckTimeout { .. })
        ));
    }

    #[tokio::test]
    async fn fails_fast_when_exited() {
        let runtime = FakeRuntime {
            status: Mutex::new(Some(ContainerStatus::Exited { exit_code: 3 })),
        };
        let err = wait_until_healthy(
            &runtime,
            &ContainerHandle::new("c"),
            Duration::from_millis(50),
        )
        .await
        .expect_err("exited container is not healthy");
        assert!(matches!(
            err,
            crate::domain::cluster::ClusterError::Docker(DockerError::ContainerNotHealthy {
                exit_code: Some(3),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn fails_fast_when_not_found() {
        let runtime = FakeRuntime {
            status: Mutex::new(Some(ContainerStatus::NotFound)),
        };
        let err = wait_until_healthy(
            &runtime,
            &ContainerHandle::new("c"),
            Duration::from_millis(50),
        )
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
}
