//! Periodic sweep that deletes `Ready` clusters past their `decommission_at` and `Failed`
//! clusters past a short grace period — "the system itself" deleting a cluster is just this
//! sweep calling [`ClusterService::request_delete`] as a direct in-process function call, using
//! the owner already known from the row being scanned, rather than a special auth bypass.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};

use crate::domain::cluster::{Cluster, ClusterState, DeleteReason};
use crate::service::cluster_service::{ClusterService, DeleteOutcome};
use crate::service::deps::TaskDeps;
use crate::service::teardown_task;

/// Determines whether `state` is due to be reaped right now, and if so, which [`DeleteReason`] to
/// record — a `Ready` cluster past its `decommission_at`, or a `Failed` cluster that's been
/// sitting for longer than `failed_grace_period` (giving callers a brief window to observe the
/// failure before it's cleaned up).
///
/// # Arguments
///
/// - `state`: the cluster's current persisted state.
/// - `now`: the current time, taken from the injected clock so this stays testable with a fake.
/// - `failed_grace_period`: how long a `Failed` cluster is left in place before being reaped.
///
/// # Returns
///
/// `Some(reason)` if the cluster should be reaped now, `None` otherwise — including for
/// `Spawning`/`Deleting` states, which this sweep never targets.
fn due_for_reaping(
    state: &ClusterState,
    now: DateTime<Utc>,
    failed_grace_period: TimeDelta,
) -> Option<DeleteReason> {
    match state {
        ClusterState::Ready {
            decommission_at, ..
        } if now >= *decommission_at => Some(DeleteReason::TtlExpired),
        ClusterState::Failed { failed_at, .. } if now >= *failed_at + failed_grace_period => {
            Some(DeleteReason::SpawnFailed)
        }
        _ => None,
    }
}

/// One sweep: scans every persisted cluster and requests deletion for any that are due. Returns
/// the clusters that were actually (or already) transitioning to `Deleting`, for the caller to
/// act on (spawn a teardown task).
///
/// # Arguments
///
/// - `cluster_service`: performs the actual delete request — the TTL/quota bookkeeping and state
///   transition live there, not duplicated here.
/// - `deps`: shared dependencies, used here to list all persisted rows and read the current time.
/// - `failed_grace_period`: passed through to [`due_for_reaping`] — how long a `Failed` cluster is
///   left in place before being reaped.
///
/// # Returns
///
/// Every cluster this sweep newly (or already) moved to `Deleting`, paired with the
/// [`DeleteOutcome`] `request_delete` returned for it. A failure to list rows, or to request
/// delete for one of them, is logged and that cluster is simply left out of the result rather than
/// propagated — this function itself never returns an error.
pub async fn run_once(
    cluster_service: &ClusterService,
    deps: &TaskDeps,
    failed_grace_period: TimeDelta,
) -> Vec<(Cluster, DeleteOutcome)> {
    let now = deps.clock.now();
    let all = match deps.repository.list_all().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(error = %error, "ttl reaper failed to list clusters");
            return Vec::new();
        }
    };

    let mut results = Vec::new();
    for cluster in all {
        let Some(reason) = due_for_reaping(&cluster.state, now, failed_grace_period) else {
            continue;
        };
        match cluster_service
            .request_delete(&cluster.id, &cluster.owner, reason)
            .await
        {
            Ok((updated, outcome)) => results.push((updated, outcome)),
            Err(error) => {
                tracing::error!(cluster_id = %cluster.id, error = %error, "ttl reaper failed to request delete");
            }
        }
    }
    results
}

/// Drives [`run_once`] on a fixed interval for the life of the process, spawning a teardown task
/// for anything it newly marked `Deleting`.
///
/// # Arguments
///
/// - `cluster_service`: passed through to each [`run_once`] sweep.
/// - `deps`: shared dependencies, passed through to each sweep and to the teardown tasks it
///   spawns.
/// - `interval`: how often to sweep.
/// - `failed_grace_period`: passed through to each [`run_once`] sweep.
pub async fn run_forever(
    cluster_service: Arc<ClusterService>,
    deps: Arc<TaskDeps>,
    interval: Duration,
    failed_grace_period: TimeDelta,
) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let outcomes = run_once(&cluster_service, &deps, failed_grace_period).await;
        for (cluster, outcome) in outcomes {
            match outcome {
                DeleteOutcome::StartTeardown => {
                    tokio::spawn(teardown_task::run(deps.clone(), cluster));
                }
                DeleteOutcome::CancelSpawn | DeleteOutcome::AlreadyDeleting => {
                    // CancelSpawn shouldn't occur here (the reaper never targets Spawning
                    // clusters), and AlreadyDeleting means another task already owns cleanup —
                    // either way, nothing for the reaper to do.
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{run_forever, run_once};
    use crate::domain::cluster::{ClusterState, DeleteReason};
    use crate::domain::ids::ClientId;
    use crate::domain::service_kind::{ConnectionInfo, ServiceKind, ServiceSpec};
    use crate::ports::clock::{Clock, FakeClock};
    use crate::redacted::Redacted;
    use crate::service::cluster_service::{ClusterService, DeleteOutcome, Limits};
    use crate::service::deps::TaskDeps;
    use crate::test_support::{
        FakeSecretGenerator, InMemoryClusterRepository, NoopPrivilegedExecutor,
    };
    use crate::worker_pool::WorkerPool;
    use chrono::{TimeDelta, Utc};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn setup() -> (ClusterService, Arc<TaskDeps>, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock::new(Utc::now()));
        let repository = Arc::new(InMemoryClusterRepository::new());
        let cluster_service = ClusterService::new(
            repository.clone(),
            clock.clone(),
            Arc::new(FakeSecretGenerator::default()),
            Limits {
                min_ttl: TimeDelta::seconds(30),
                max_ttl: TimeDelta::seconds(3600),
                max_clusters_per_user: 10,
            },
        );
        let deps = Arc::new(TaskDeps {
            repository,
            worker_pool: Arc::new(WorkerPool::new(vec![])),
            privileged_exec: Arc::new(NoopPrivilegedExecutor),
            backends: HashMap::new(),
            clock: clock.clone(),
            worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
        });
        (cluster_service, deps, clock)
    }

    #[tokio::test]
    async fn spawning_cluster_is_never_reaped() {
        let (service, deps, _clock) = setup();
        service
            .create(
                ClientId::new("agent"),
                ServiceSpec {
                    kind: ServiceKind::Postgres,
                    pgvector: false,
                },
                TimeDelta::seconds(30),
            )
            .await
            .expect("create");

        let outcomes = run_once(&service, &deps, TimeDelta::seconds(5)).await;
        assert!(outcomes.is_empty());
    }

    #[tokio::test]
    async fn ready_cluster_past_decommission_time_is_reaped() {
        let (service, deps, clock) = setup();
        let owner = ClientId::new("agent");
        let created = service
            .create(
                owner.clone(),
                ServiceSpec {
                    kind: ServiceKind::Postgres,
                    pgvector: false,
                },
                TimeDelta::seconds(30),
            )
            .await
            .expect("create");

        let ready_at = clock.now();
        let decommission_at = ready_at + TimeDelta::seconds(30);
        deps.repository
            .update_state(
                &created.id,
                &ClusterState::Ready {
                    ready_at,
                    decommission_at,
                    connection: ConnectionInfo {
                        host: "127.0.0.1".to_string(),
                        port: 5432,
                        dbname: "app_salmon".to_string(),
                        user: "app_salmon".to_string(),
                        password: Redacted::new("hunter2".to_string()),
                    },
                },
            )
            .await
            .expect("mark ready");

        // Not yet due.
        let outcomes = run_once(&service, &deps, TimeDelta::seconds(5)).await;
        assert!(outcomes.is_empty());

        clock.advance(TimeDelta::seconds(31));
        let outcomes = run_once(&service, &deps, TimeDelta::seconds(5)).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].1, DeleteOutcome::StartTeardown);
        assert!(matches!(
            outcomes[0].0.state,
            ClusterState::Deleting {
                reason: DeleteReason::TtlExpired,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn failed_cluster_is_reaped_after_grace_period() {
        let (service, deps, clock) = setup();
        let created = service
            .create(
                ClientId::new("agent"),
                ServiceSpec {
                    kind: ServiceKind::Postgres,
                    pgvector: false,
                },
                TimeDelta::seconds(30),
            )
            .await
            .expect("create");

        deps.repository
            .update_state(
                &created.id,
                &ClusterState::Failed {
                    failed_at: clock.now(),
                    error_summary: "boom".to_string(),
                },
            )
            .await
            .expect("mark failed");

        let outcomes = run_once(&service, &deps, TimeDelta::seconds(5)).await;
        assert!(outcomes.is_empty(), "not yet past grace period");

        clock.advance(TimeDelta::seconds(6));
        let outcomes = run_once(&service, &deps, TimeDelta::seconds(5)).await;
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            outcomes[0].0.state,
            ClusterState::Deleting {
                reason: DeleteReason::SpawnFailed,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_cluster_already_moved_to_deleting_is_not_reswept() {
        let (service, deps, clock) = setup();
        let owner = ClientId::new("agent");
        let created = service
            .create(
                owner.clone(),
                ServiceSpec {
                    kind: ServiceKind::Postgres,
                    pgvector: false,
                },
                TimeDelta::seconds(30),
            )
            .await
            .expect("create");
        let ready_at = clock.now();
        deps.repository
            .update_state(
                &created.id,
                &ClusterState::Ready {
                    ready_at,
                    decommission_at: ready_at,
                    connection: ConnectionInfo {
                        host: "127.0.0.1".to_string(),
                        port: 5432,
                        dbname: "app_salmon".to_string(),
                        user: "app_salmon".to_string(),
                        password: Redacted::new("hunter2".to_string()),
                    },
                },
            )
            .await
            .expect("mark ready, already past decommission");

        let first = run_once(&service, &deps, TimeDelta::seconds(5)).await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].1, DeleteOutcome::StartTeardown);

        // The row is now `Deleting`, which `due_for_reaping` never matches — so a second sweep
        // (e.g. the next timer tick, before a teardown task has actually removed the row) does
        // not call `request_delete` on it again at all.
        let second = run_once(&service, &deps, TimeDelta::seconds(5)).await;
        assert!(second.is_empty());
    }

    #[tokio::test]
    async fn run_forever_reaps_and_spawns_teardown_on_each_tick() {
        let (service, deps, clock) = setup();
        let service = Arc::new(service);
        let owner = ClientId::new("agent");
        let created = service
            .create(
                owner.clone(),
                ServiceSpec {
                    kind: ServiceKind::Postgres,
                    pgvector: false,
                },
                TimeDelta::seconds(30),
            )
            .await
            .expect("create");

        let ready_at = clock.now();
        deps.repository
            .update_state(
                &created.id,
                &ClusterState::Ready {
                    ready_at,
                    decommission_at: ready_at,
                    connection: ConnectionInfo {
                        host: "127.0.0.1".to_string(),
                        port: 5432,
                        dbname: "app_salmon".to_string(),
                        user: "app_salmon".to_string(),
                        password: Redacted::new("hunter2".to_string()),
                    },
                },
            )
            .await
            .expect("mark ready, already past decommission");

        let handle = tokio::spawn(run_forever(
            service,
            deps.clone(),
            std::time::Duration::from_millis(20),
            TimeDelta::seconds(5),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        handle.abort();

        // The fake setup's teardown (no registered backend, no worker) completes almost
        // instantly, so by now the row should be fully gone rather than caught mid-`Deleting`.
        let stored = deps.repository.get_any(&created.id).await.expect("query");
        assert!(
            stored.is_none(),
            "run_forever should have reaped and fully torn down the expired cluster by now"
        );
    }
}
