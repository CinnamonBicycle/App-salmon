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
/// What "ready" means depends on `requires_healthcheck`, and deliberately isn't inferred from the
/// `health` value itself: the real Docker Engine API reports an entirely *absent* `.State.Health`
/// field (which `adapters::docker_bollard` surfaces as `health: None`, not
/// `Some(HealthState::None)`) for a container with no `HEALTHCHECK` configured at all — the same
/// shape a fake/adapter might otherwise use to mean "no information yet." Trying to distinguish
/// those two cases from the `health` value alone is exactly the ambiguity that produced a real
/// bug here (a `HealthState::None` variant that real bollard output never actually produces) —
/// the caller already knows, at the point it built the `ContainerSpec`, whether it asked for a
/// `HEALTHCHECK`, so it just says so directly instead.
///
/// - `requires_healthcheck: true`: ready only once `health` is exactly `Some(HealthState::Healthy)`.
/// - `requires_healthcheck: false`: ready as soon as the container is `Running` at all, regardless
///   of what (if anything) `health` reports — there's no `HEALTHCHECK` to wait on, so `Running` is
///   the best signal available. Callers using this should independently confirm (empirically, not
///   assumed) that the in-container service is actually accepting requests by the time it reports
///   `Running`, or add a real `HEALTHCHECK` instead — see `docs/DESIGN.md` §11's M4b placeholders.
///
/// Any other status while `requires_healthcheck` is true — `Starting`, `Unhealthy`, or no health
/// information yet — just means "keep polling": Docker itself retries a failing check before
/// marking a container `unhealthy`, and can recover it to `healthy` on a later check, so this
/// helper's own timeout (not a single bad observation) is what ultimately bounds how long a
/// caller waits.
///
/// # Arguments
///
/// - `container_runtime`: how to inspect `handle`'s current status.
/// - `handle`: the container to poll.
/// - `timeout`: the overall deadline to wait before giving up.
/// - `requires_healthcheck`: whether `handle` was created with a `HEALTHCHECK` to wait on — see
///   above.
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
    requires_healthcheck: bool,
) -> Result<Option<u16>, ClusterError> {
    let deadline = Instant::now() + timeout;
    loop {
        match container_runtime.inspect(handle).await? {
            ContainerStatus::Running {
                published_port,
                health,
            } => {
                let ready = if requires_healthcheck {
                    matches!(health, Some(HealthState::Healthy))
                } else {
                    true
                };
                if ready {
                    return Ok(published_port);
                }
            }
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
            true,
        )
        .await
        .expect("healthy");
        assert_eq!(port, Some(55432));
    }

    #[tokio::test]
    async fn succeeds_as_soon_as_running_when_no_healthcheck_was_requested() {
        // The real Docker Engine API reports an entirely absent `.State.Health` for a container
        // with no HEALTHCHECK configured (surfaced here as `health: None`, not
        // `Some(HealthState::None)`) — this is the case `requires_healthcheck: false` exists for,
        // and it must not depend on the `health` value at all, only on `Running`.
        let runtime = FakeRuntime {
            status: Mutex::new(Some(ContainerStatus::Running {
                published_port: None,
                health: None,
            })),
        };
        let port = wait_until_healthy(
            &runtime,
            &ContainerHandle::new("c"),
            Duration::from_millis(50),
            false,
        )
        .await
        .expect("no-healthcheck container is ready as soon as it's running");
        assert_eq!(port, None);
    }

    #[tokio::test]
    async fn times_out_when_a_healthcheck_is_required_but_health_information_never_arrives() {
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
            true,
        )
        .await
        .expect_err("no health info at all never resolves as ready when one was required");
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
            true,
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
            true,
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
            true,
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
