//! The cluster lifecycle state machine.
//!
//! State lives as a plain enum field on [`Cluster`], not as distinct Rust types per state
//! (typestate), because independent tasks — the HTTP handler, the background spawn task, the
//! TTL reaper, and startup reconciliation after a restart — mutate the *same persisted row* from
//! different call stacks (and, across a restart, different process lifetimes). A compile-time
//! type can't reflect what's true in another task or in `SQLite`. What Rust's type system buys
//! instead is [`transition`]: a pure function with an exhaustive match and no wildcard arm, so
//! adding a state or event later forces every call site to be revisited by the compiler.
//!
//! `Gone` is deliberately not a variant. It's the absence of a row: teardown's last act is
//! deleting the row, which is what flips `GET /clusters/{id}` from `410 Gone` to `404 Not Found`.

use chrono::{DateTime, TimeDelta, Utc};
use thiserror::Error;

use crate::client_workers::ClientWorkerError;
use crate::domain::ids::{ClientId, ClusterId, WorkerUser};
use crate::domain::service_kind::{ConnectionInfo, ServiceSpec};
use crate::ports::container_runtime::DockerError;
use crate::ports::repository::RepositoryError;

/// A single provisioned (or provisioning) cluster row, as persisted by `ports::repository` and
/// returned to API callers.
#[derive(Debug, Clone)]
pub struct Cluster {
    /// Unique, sortable-by-creation-time identifier; also the path segment in
    /// `GET /clusters/{id}`.
    pub id: ClusterId,
    /// The client account that created this cluster — every lookup is scoped to this to keep
    /// "doesn't exist" and "exists but isn't yours" indistinguishable.
    pub owner: ClientId,
    /// What kind of backend this cluster is (and any backend-specific options, e.g. `pgvector`).
    pub service: ServiceSpec,
    /// The TTL the caller asked for at creation time; combined with `ready_at` once the cluster
    /// becomes `Ready` to compute `decommission_at`.
    pub requested_ttl: TimeDelta,
    /// When the creation request was accepted, before any provisioning work started.
    pub requested_at: DateTime<Utc>,
    /// Where this cluster currently is in its lifecycle — see [`ClusterState`].
    pub state: ClusterState,
    /// `Some` from the moment the owner's account is resolved during spawn — persisted on the row
    /// (rather than re-derived from `owner` on every use) so teardown can still find and wipe the
    /// right directory even if the operator's client config changes between a cluster's spawn and
    /// its teardown. Outlives the `Spawning` state: a cluster that fails or is deleted mid-spawn
    /// still needs it until cleanup actually completes.
    pub worker: Option<WorkerUser>,
    /// Which of the owner's `max_clusters_per_user` directory slots (`0..limit`) this cluster's
    /// on-disk directory uses — see `client_workers::worker_data_dir`. Assigned atomically by
    /// [`crate::ports::repository::ClusterRepository::try_insert_if_under_quota`] at insert time
    /// (smallest slot not already used by one of the owner's other active rows), so it's always a
    /// real, distinct value for any row read back from storage; a value built locally before that
    /// call (see `service::cluster_service::ClusterService::create`) is a placeholder the
    /// repository always overwrites, never one a caller should rely on. Fixed, literal per-slot
    /// paths (rather than one path per cluster id) are what let the sudoers rule enumerate exactly
    /// `max_clusters_per_user` allowed paths per client instead of needing a wildcard — some
    /// `sudo` implementations (`sudo-rs`, confirmed via `visudo -c`) reject wildcards embedded in
    /// command arguments outright, so the path must be one of a small, literal, pre-provisioned
    /// set.
    pub slot: u32,
}

/// Where a [`Cluster`] currently is in its lifecycle. See the module docs for why this is a plain
/// enum field rather than typestate, and why there is no `Gone` variant.
#[derive(Debug, Clone, PartialEq)]
pub enum ClusterState {
    /// Provisioning is in progress (worker allocation, container create/start, health checks).
    /// No process may be actively working on this row (e.g. after a crash) — startup
    /// reconciliation treats a `Spawning` row it finds at boot as abandoned.
    Spawning {
        /// When this cluster entered `Spawning` — i.e. when the create request was accepted.
        started_at: DateTime<Utc>,
    },
    /// The cluster is up and its connection details are available to callers.
    Ready {
        /// When the backend actually finished provisioning and became reachable.
        ready_at: DateTime<Utc>,
        /// The TTL anchor: `ready_at + requested_ttl`, fixed the moment the cluster becomes
        /// ready. `GET` merely reports this; polling for it does not delay it.
        decommission_at: DateTime<Utc>,
        /// How to connect to the running service.
        connection: ConnectionInfo,
    },
    /// Provisioning failed; the cluster still holds its worker until the reaper's grace period
    /// passes, then it's torn down like any other cluster.
    Failed {
        /// When the failure was observed.
        failed_at: DateTime<Utc>,
        /// Sanitized: a small closed set of category strings (see `backends::postgres`), never
        /// raw subprocess stderr or Docker error text, so a secret can't leak through here.
        error_summary: String,
    },
    /// Teardown is in progress (or queued) — backend resources are being released, then the
    /// worker, then the row itself is deleted, which is what flips `GET` from `410` to `404`.
    Deleting {
        /// When deletion was requested (or, for an idempotent re-request, when it was first
        /// requested).
        deleting_since: DateTime<Utc>,
        /// Why this cluster is being deleted — user action, TTL expiry, a failed spawn being
        /// reaped, or reconciliation giving up on it after a restart.
        reason: DeleteReason,
    },
}

/// Why a cluster transitioned to [`ClusterState::Deleting`]. Persisted alongside the row (see
/// `adapters::sqlite_repository`) so it survives a restart and is visible to `GET`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteReason {
    /// The owning client called `DELETE /clusters/{id}`.
    UserRequested,
    /// The TTL reaper found this cluster past its `decommission_at`.
    TtlExpired,
    /// The TTL reaper found this `Failed` cluster past its grace period.
    SpawnFailed,
    /// Reconciliation gave up on a row it can't safely resume after a restart (see
    /// `service::reconciliation`).
    AdminForced,
}

/// An input to [`transition`] — something that happened to a cluster that may change its state.
#[derive(Debug, Clone, PartialEq)]
pub enum ClusterEvent {
    /// The backend finished provisioning and the cluster is reachable.
    SpawnSucceeded {
        /// When the backend became reachable.
        ready_at: DateTime<Utc>,
        /// The computed TTL anchor (`ready_at + requested_ttl`) to store on the resulting
        /// [`ClusterState::Ready`].
        decommission_at: DateTime<Utc>,
        /// How to connect to the now-running service.
        connection: ConnectionInfo,
    },
    /// Provisioning failed and will not be retried.
    SpawnFailed {
        /// When the failure was observed.
        failed_at: DateTime<Utc>,
        /// A sanitized summary safe to store and return to callers.
        error_summary: String,
    },
    /// Someone (a caller, the TTL reaper, or reconciliation) wants this cluster torn down.
    DeleteRequested {
        /// When the request happened.
        at: DateTime<Utc>,
        /// Why, for [`ClusterState::Deleting::reason`].
        reason: DeleteReason,
    },
}

/// Errors from cluster lifecycle operations — what `service::cluster_service`'s callers match on.
#[derive(Debug, Error)]
pub enum ClusterError {
    /// The caller's requested TTL fell outside the configured `[min_secs, max_secs]` bounds.
    #[error("requested TTL {requested_secs}s outside [{min_secs}, {max_secs}]")]
    TtlOutOfBounds {
        /// The TTL (in seconds) the caller asked for.
        requested_secs: i64,
        /// The configured minimum allowed TTL, in seconds.
        min_secs: i64,
        /// The configured maximum allowed TTL, in seconds.
        max_secs: i64,
    },
    /// `owner` already has `limit` active clusters and cannot create another.
    #[error("owner {owner} already has {count} active clusters (limit {limit})")]
    QuotaExceeded {
        /// The client account that hit its quota.
        owner: ClientId,
        /// How many active clusters `owner` currently has.
        count: u32,
        /// The configured per-owner limit.
        limit: u32,
    },
    /// No row exists for this id (or, at the repository layer, it exists but isn't owned by the
    /// caller — see `ports::repository::ClusterRepository::get_owned`).
    #[error("cluster {0} not found")]
    NotFound(ClusterId),
    /// [`transition`] was asked to apply an event that doesn't apply to the current state.
    #[error("state {from:?} cannot handle event {event:?}")]
    InvalidTransition {
        /// The state `transition` was called with.
        from: Box<ClusterState>,
        /// The event that didn't apply to it.
        event: Box<ClusterEvent>,
    },
    /// Resolving the owner's Unix account, or a directory prepare/wipe operation against it,
    /// failed.
    #[error(transparent)]
    ClientWorker(#[from] ClientWorkerError),
    /// A container-runtime (Docker) operation failed.
    #[error(transparent)]
    Docker(#[from] DockerError),
    /// The durable storage layer failed.
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    /// A backend-specific setup step failed after the container itself came up healthy (e.g.
    /// `backends::postgres` enabling the `pgvector` extension). The message is a sanitized,
    /// backend-chosen summary — never raw driver/subprocess output — since it ends up in
    /// [`ClusterState::Failed::error_summary`].
    #[error("backend failed to become ready: {0}")]
    BackendSpawnFailed(String),
}

/// Computes the next state of a cluster given its current state and something that happened to
/// it. Pure: no I/O, no clock reads (callers supply timestamps), fully unit-testable. Every arm is
/// exhaustive — there is no `_ =>` wildcard, so a new [`ClusterState`] or [`ClusterEvent`]
/// variant fails to compile here until every arm is reconsidered.
///
/// # Arguments
///
/// - `current`: the cluster's state before `event`.
/// - `event`: what happened to the cluster.
///
/// # Returns
///
/// The cluster's new state after applying `event`. A `DeleteRequested` event applied to a
/// cluster already in [`ClusterState::Deleting`] is idempotent and returns `current` unchanged
/// (cloned).
///
/// # Errors
///
/// Returns [`ClusterError::InvalidTransition`] if `event` does not apply to `current` (e.g. a
/// spawn-completion event arriving for a cluster that is already `Ready` or `Deleting`).
pub fn transition(
    current: &ClusterState,
    event: ClusterEvent,
) -> Result<ClusterState, ClusterError> {
    match (current, event) {
        (
            ClusterState::Spawning { .. },
            ClusterEvent::SpawnSucceeded {
                ready_at,
                decommission_at,
                connection,
            },
        ) => Ok(ClusterState::Ready {
            ready_at,
            decommission_at,
            connection,
        }),
        (
            ClusterState::Spawning { .. },
            ClusterEvent::SpawnFailed {
                failed_at,
                error_summary,
            },
        ) => Ok(ClusterState::Failed {
            failed_at,
            error_summary,
        }),
        (
            ClusterState::Spawning { .. }
            | ClusterState::Ready { .. }
            | ClusterState::Failed { .. },
            ClusterEvent::DeleteRequested { at, reason },
        ) => Ok(ClusterState::Deleting {
            deleting_since: at,
            reason,
        }),
        (ClusterState::Deleting { .. }, ClusterEvent::DeleteRequested { .. }) => {
            Ok(current.clone())
        }
        (
            ClusterState::Ready { .. }
            | ClusterState::Failed { .. }
            | ClusterState::Deleting { .. },
            event @ (ClusterEvent::SpawnSucceeded { .. } | ClusterEvent::SpawnFailed { .. }),
        ) => Err(ClusterError::InvalidTransition {
            from: Box::new(current.clone()),
            event: Box::new(event),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{ClusterError, ClusterEvent, ClusterState, DeleteReason, transition};
    use crate::domain::service_kind::{ConnectionInfo, PostgresConnectionInfo};
    use crate::redacted::Redacted;
    use chrono::Utc;

    fn sample_connection() -> ConnectionInfo {
        ConnectionInfo::Postgres(PostgresConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: 5432,
            dbname: "app".to_string(),
            user: "app".to_string(),
            password: Redacted::new("hunter2".to_string()),
        })
    }

    fn spawning() -> ClusterState {
        ClusterState::Spawning {
            started_at: Utc::now(),
        }
    }

    fn ready() -> ClusterState {
        let now = Utc::now();
        ClusterState::Ready {
            ready_at: now,
            decommission_at: now,
            connection: sample_connection(),
        }
    }

    fn failed() -> ClusterState {
        ClusterState::Failed {
            failed_at: Utc::now(),
            error_summary: "image pull failed".to_string(),
        }
    }

    fn deleting() -> ClusterState {
        ClusterState::Deleting {
            deleting_since: Utc::now(),
            reason: DeleteReason::UserRequested,
        }
    }

    fn spawn_succeeded() -> ClusterEvent {
        let now = Utc::now();
        ClusterEvent::SpawnSucceeded {
            ready_at: now,
            decommission_at: now,
            connection: sample_connection(),
        }
    }

    fn spawn_failed() -> ClusterEvent {
        ClusterEvent::SpawnFailed {
            failed_at: Utc::now(),
            error_summary: "health check timeout".to_string(),
        }
    }

    fn delete_requested(reason: DeleteReason) -> ClusterEvent {
        ClusterEvent::DeleteRequested {
            at: Utc::now(),
            reason,
        }
    }

    #[test]
    fn spawning_plus_spawn_succeeded_becomes_ready() {
        let result = transition(&spawning(), spawn_succeeded()).expect("valid transition");
        assert!(matches!(result, ClusterState::Ready { .. }));
    }

    #[test]
    fn spawning_plus_spawn_failed_becomes_failed() {
        let result = transition(&spawning(), spawn_failed()).expect("valid transition");
        assert!(matches!(result, ClusterState::Failed { .. }));
    }

    #[test]
    fn spawning_plus_delete_requested_becomes_deleting() {
        let result = transition(&spawning(), delete_requested(DeleteReason::UserRequested))
            .expect("valid transition");
        assert!(matches!(
            result,
            ClusterState::Deleting {
                reason: DeleteReason::UserRequested,
                ..
            }
        ));
    }

    #[test]
    fn ready_plus_delete_requested_becomes_deleting() {
        let result = transition(&ready(), delete_requested(DeleteReason::TtlExpired))
            .expect("valid transition");
        assert!(matches!(
            result,
            ClusterState::Deleting {
                reason: DeleteReason::TtlExpired,
                ..
            }
        ));
    }

    #[test]
    fn failed_plus_delete_requested_becomes_deleting() {
        let result = transition(&failed(), delete_requested(DeleteReason::SpawnFailed))
            .expect("valid transition");
        assert!(matches!(
            result,
            ClusterState::Deleting {
                reason: DeleteReason::SpawnFailed,
                ..
            }
        ));
    }

    #[test]
    fn deleting_plus_delete_requested_is_idempotent() {
        let original = deleting();
        let result = transition(&original, delete_requested(DeleteReason::UserRequested))
            .expect("idempotent delete");
        assert_eq!(result, original);
    }

    #[test]
    fn ready_plus_spawn_succeeded_is_invalid() {
        let err = transition(&ready(), spawn_succeeded()).expect_err("invalid transition");
        assert!(matches!(err, ClusterError::InvalidTransition { .. }));
    }

    #[test]
    fn ready_plus_spawn_failed_is_invalid() {
        let err = transition(&ready(), spawn_failed()).expect_err("invalid transition");
        assert!(matches!(err, ClusterError::InvalidTransition { .. }));
    }

    #[test]
    fn failed_plus_spawn_succeeded_is_invalid() {
        let err = transition(&failed(), spawn_succeeded()).expect_err("invalid transition");
        assert!(matches!(err, ClusterError::InvalidTransition { .. }));
    }

    #[test]
    fn failed_plus_spawn_failed_is_invalid() {
        let err = transition(&failed(), spawn_failed()).expect_err("invalid transition");
        assert!(matches!(err, ClusterError::InvalidTransition { .. }));
    }

    #[test]
    fn deleting_plus_spawn_succeeded_is_invalid() {
        let err = transition(&deleting(), spawn_succeeded()).expect_err("invalid transition");
        assert!(matches!(err, ClusterError::InvalidTransition { .. }));
    }

    #[test]
    fn deleting_plus_spawn_failed_is_invalid() {
        let err = transition(&deleting(), spawn_failed()).expect_err("invalid transition");
        assert!(matches!(err, ClusterError::InvalidTransition { .. }));
    }

    #[test]
    fn cluster_error_display_messages_are_stable() {
        assert_eq!(
            ClusterError::TtlOutOfBounds {
                requested_secs: 5,
                min_secs: 30,
                max_secs: 3600
            }
            .to_string(),
            "requested TTL 5s outside [30, 3600]"
        );
        let owner = crate::domain::ids::ClientId::new("agent");
        assert_eq!(
            ClusterError::QuotaExceeded {
                owner: owner.clone(),
                count: 2,
                limit: 2
            }
            .to_string(),
            "owner agent already has 2 active clusters (limit 2)"
        );
        let id = crate::domain::ids::ClusterId::new(ulid::Ulid::nil());
        assert_eq!(
            ClusterError::NotFound(id).to_string(),
            format!("cluster {} not found", ulid::Ulid::nil())
        );
    }
}
