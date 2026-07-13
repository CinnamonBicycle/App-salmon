//! Orchestrates the 4 endpoints' business rules: TTL bounds, the atomic quota check, state
//! transitions, and repository reads/writes. Deliberately does not touch `ClientWorkers`,
//! `ContainerRuntime`, or `PrivilegedExecutor` — actually provisioning/tearing down a cluster is
//! `service::spawn_task`/`service::teardown_task`'s job, kicked off by the caller (an HTTP
//! handler, or the TTL reaper) after this service confirms the state transition is valid. That
//! split is what makes this layer testable purely against fakes, with no background tasks
//! involved.

use std::sync::Arc;

use chrono::TimeDelta;

use crate::domain::cluster::{
    Cluster, ClusterError, ClusterEvent, ClusterState, DeleteReason, transition,
};
use crate::domain::ids::{ClientId, ClusterId};
use crate::domain::service_kind::ServiceSpec;
use crate::ports::clock::Clock;
use crate::ports::repository::{ClusterRepository, InsertOutcome};
use crate::ports::secrets::SecretGenerator;

/// Configured bounds `ClusterService` enforces on every create/delete request.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Shortest TTL a caller may request; a shorter request is rejected with
    /// [`ClusterError::TtlOutOfBounds`].
    pub min_ttl: TimeDelta,
    /// Longest TTL a caller may request; a longer request is rejected with
    /// [`ClusterError::TtlOutOfBounds`].
    pub max_ttl: TimeDelta,
    /// How many non-absent clusters (`Spawning`/`Ready`/`Failed`/`Deleting`) a single owner may
    /// have at once, enforced atomically by [`crate::ports::repository::ClusterRepository::try_insert_if_under_quota`].
    pub max_clusters_per_user: u32,
}

/// What the caller should do next after a delete request, so exactly one task ever ends up
/// tearing a given cluster down:
/// - `CancelSpawn`: the cluster was `Spawning` — cancel its in-flight `spawn_task` (via the
///   caller's `CancellationToken` registry); that task tears down whatever it had already
///   allocated itself. The caller must *not* also start a fresh `teardown_task`.
/// - `StartTeardown`: the cluster was `Ready` or `Failed` — nothing else is touching its
///   resources, so the caller should start a new `teardown_task`.
/// - `AlreadyDeleting`: some task is already handling this cluster's teardown; the caller does
///   nothing further.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// The cluster was `Spawning` — the caller must cancel its in-flight `spawn_task` (via its
    /// `CancellationToken` registry) and must *not* also start a fresh `teardown_task`; the
    /// cancelled spawn task tears down whatever it had already allocated itself.
    CancelSpawn,
    /// The cluster was `Ready` or `Failed` — nothing else is touching its resources, so the
    /// caller should start a new `teardown_task`.
    StartTeardown,
    /// Some task is already handling this cluster's teardown; the caller does nothing further.
    AlreadyDeleting,
}

pub struct ClusterService {
    /// Durable store of cluster rows.
    repository: Arc<dyn ClusterRepository>,
    /// Source of the current time, used to stamp `requested_at`/delete timestamps and injectable
    /// for deterministic tests.
    clock: Arc<dyn Clock>,
    /// Source of new cluster IDs, injectable for deterministic tests.
    secrets: Arc<dyn SecretGenerator>,
    /// TTL and quota bounds this service enforces.
    limits: Limits,
}

impl ClusterService {
    /// Builds a `ClusterService` from its dependencies.
    ///
    /// # Arguments
    ///
    /// - `repository`: durable store used for every read/write of cluster rows.
    /// - `clock`: time source used to stamp requests and compute delete timestamps.
    /// - `secrets`: source of new cluster IDs.
    /// - `limits`: the TTL and per-owner quota bounds to enforce.
    ///
    /// # Returns
    ///
    /// A ready-to-use `ClusterService`.
    #[must_use]
    pub fn new(
        repository: Arc<dyn ClusterRepository>,
        clock: Arc<dyn Clock>,
        secrets: Arc<dyn SecretGenerator>,
        limits: Limits,
    ) -> Self {
        Self {
            repository,
            clock,
            secrets,
            limits,
        }
    }

    /// Validates the requested TTL, then atomically checks the owner's quota and inserts a new
    /// `Spawning` cluster row. Does not itself provision any backend resources — the caller (an
    /// HTTP handler) is responsible for kicking off `service::spawn_task::run` afterward.
    ///
    /// # Arguments
    ///
    /// - `owner`: the client requesting the cluster; also the quota scope.
    /// - `service`: which backend kind to spawn (and any backend-specific options, e.g.
    ///   `pgvector`).
    /// - `requested_ttl`: how long the cluster should live once ready; validated against
    ///   [`Limits::min_ttl`]/[`Limits::max_ttl`].
    ///
    /// # Returns
    ///
    /// The newly created `Cluster`, in `Spawning` state.
    ///
    /// # Errors
    ///
    /// [`ClusterError::TtlOutOfBounds`] if `requested_ttl` is outside the configured range,
    /// [`ClusterError::QuotaExceeded`] if `owner` is already at their cluster limit, or a wrapped
    /// [`crate::ports::repository::RepositoryError`] on a storage failure.
    pub async fn create(
        &self,
        owner: ClientId,
        service: ServiceSpec,
        requested_ttl: TimeDelta,
    ) -> Result<Cluster, ClusterError> {
        if requested_ttl < self.limits.min_ttl || requested_ttl > self.limits.max_ttl {
            return Err(ClusterError::TtlOutOfBounds {
                requested_secs: requested_ttl.num_seconds(),
                min_secs: self.limits.min_ttl.num_seconds(),
                max_secs: self.limits.max_ttl.num_seconds(),
            });
        }

        let now = self.clock.now();
        let cluster = Cluster {
            id: self.secrets.cluster_id(),
            owner: owner.clone(),
            service,
            requested_ttl,
            requested_at: now,
            state: ClusterState::Spawning { started_at: now },
            worker: None,
            // Placeholder — `try_insert_if_under_quota` ignores this and assigns the real slot
            // atomically, returned below.
            slot: 0,
        };

        match self
            .repository
            .try_insert_if_under_quota(&cluster, self.limits.max_clusters_per_user)
            .await?
        {
            InsertOutcome::Inserted { slot } => Ok(Cluster { slot, ..cluster }),
            InsertOutcome::QuotaExceeded { current_count } => Err(ClusterError::QuotaExceeded {
                owner,
                count: current_count,
                limit: self.limits.max_clusters_per_user,
            }),
        }
    }

    /// Looks up a single cluster, scoped to its owner — a cluster that exists but belongs to
    /// someone else is indistinguishable from one that never existed, by design (see
    /// `ClusterRepository::get_owned`), so this never leaks existence to a non-owner.
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster to look up.
    /// - `owner`: the caller; only a cluster owned by this client is returned.
    ///
    /// # Returns
    ///
    /// The matching `Cluster`, in whatever state it currently has.
    ///
    /// # Errors
    ///
    /// [`ClusterError::NotFound`] if no cluster with `id` is owned by `owner`.
    pub async fn info(&self, id: &ClusterId, owner: &ClientId) -> Result<Cluster, ClusterError> {
        self.repository
            .get_owned(id, owner)
            .await?
            .ok_or(ClusterError::NotFound(*id))
    }

    /// Lists every non-absent cluster owned by `owner`, in any state.
    ///
    /// # Arguments
    ///
    /// - `owner`: the caller whose clusters to list.
    ///
    /// # Returns
    ///
    /// All of `owner`'s clusters, in no particular order.
    ///
    /// # Errors
    ///
    /// A wrapped [`crate::ports::repository::RepositoryError`] on a storage failure.
    pub async fn list(&self, owner: &ClientId) -> Result<Vec<Cluster>, ClusterError> {
        Ok(self.repository.list_by_owner(owner).await?)
    }

    /// Transitions a cluster to `Deleting`, idempotently. Used both by the owner-authenticated
    /// `DELETE` handler and by the TTL reaper (which already knows the correct `owner` from the
    /// row it's scanning, so it reuses this same owner-scoped method rather than needing a
    /// separate unscoped delete path). Does not itself tear anything down — the returned
    /// [`DeleteOutcome`] tells the caller exactly what follow-up action to take so that exactly
    /// one task ever ends up doing the teardown.
    ///
    /// # Arguments
    ///
    /// - `id`: the cluster to delete.
    /// - `owner`: the caller; only a cluster owned by this client can be deleted.
    /// - `reason`: why the deletion was requested, persisted on the `Deleting` state.
    ///
    /// # Returns
    ///
    /// The cluster with its state now `Deleting` (or already `Deleting`, if this call is a
    /// repeat), paired with a [`DeleteOutcome`] telling the caller what to do next.
    ///
    /// # Errors
    ///
    /// [`ClusterError::NotFound`] if no cluster with `id` is owned by `owner`.
    pub async fn request_delete(
        &self,
        id: &ClusterId,
        owner: &ClientId,
        reason: DeleteReason,
    ) -> Result<(Cluster, DeleteOutcome), ClusterError> {
        let cluster = self
            .repository
            .get_owned(id, owner)
            .await?
            .ok_or(ClusterError::NotFound(*id))?;
        let was_already_deleting = matches!(cluster.state, ClusterState::Deleting { .. });
        let was_spawning = matches!(cluster.state, ClusterState::Spawning { .. });

        let new_state = transition(
            &cluster.state,
            ClusterEvent::DeleteRequested {
                at: self.clock.now(),
                reason,
            },
        )?;
        self.repository.update_state(id, &new_state).await?;

        let outcome = if was_already_deleting {
            DeleteOutcome::AlreadyDeleting
        } else if was_spawning {
            DeleteOutcome::CancelSpawn
        } else {
            DeleteOutcome::StartTeardown
        };
        Ok((
            Cluster {
                state: new_state,
                ..cluster
            },
            outcome,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{ClusterService, DeleteOutcome, Limits};
    use crate::domain::cluster::{ClusterError, ClusterState, DeleteReason};
    use crate::domain::ids::ClientId;
    use crate::domain::service_kind::{ServiceKind, ServiceSpec};
    use crate::ports::clock::{Clock, FakeClock};
    use crate::test_support::{FakeSecretGenerator, InMemoryClusterRepository};
    use chrono::{TimeDelta, Utc};
    use std::sync::Arc;

    fn service_with_limits(limits: Limits) -> (ClusterService, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock::new(Utc::now()));
        let service = ClusterService::new(
            Arc::new(InMemoryClusterRepository::new()),
            clock.clone(),
            Arc::new(FakeSecretGenerator::default()),
            limits,
        );
        (service, clock)
    }

    fn default_limits() -> Limits {
        Limits {
            min_ttl: TimeDelta::seconds(30),
            max_ttl: TimeDelta::seconds(3600),
            max_clusters_per_user: 2,
        }
    }

    fn postgres_spec() -> ServiceSpec {
        ServiceSpec {
            kind: ServiceKind::Postgres,
            pgvector: false,
        }
    }

    #[tokio::test]
    async fn create_succeeds_within_ttl_bounds() {
        let (service, _clock) = service_with_limits(default_limits());
        let cluster = service
            .create(
                ClientId::new("agent"),
                postgres_spec(),
                TimeDelta::seconds(300),
            )
            .await
            .expect("valid create");
        assert!(matches!(cluster.state, ClusterState::Spawning { .. }));
    }

    #[tokio::test]
    async fn create_rejects_ttl_below_minimum() {
        let (service, _clock) = service_with_limits(default_limits());
        let err = service
            .create(
                ClientId::new("agent"),
                postgres_spec(),
                TimeDelta::seconds(5),
            )
            .await
            .expect_err("ttl too low");
        assert!(matches!(err, ClusterError::TtlOutOfBounds { .. }));
    }

    #[tokio::test]
    async fn create_rejects_ttl_above_maximum() {
        let (service, _clock) = service_with_limits(default_limits());
        let err = service
            .create(
                ClientId::new("agent"),
                postgres_spec(),
                TimeDelta::seconds(7200),
            )
            .await
            .expect_err("ttl too high");
        assert!(matches!(err, ClusterError::TtlOutOfBounds { .. }));
    }

    #[tokio::test]
    async fn create_enforces_quota_per_owner() {
        let (service, _clock) = service_with_limits(default_limits());
        let owner = ClientId::new("agent");
        service
            .create(owner.clone(), postgres_spec(), TimeDelta::seconds(60))
            .await
            .expect("1st");
        service
            .create(owner.clone(), postgres_spec(), TimeDelta::seconds(60))
            .await
            .expect("2nd");
        let err = service
            .create(owner.clone(), postgres_spec(), TimeDelta::seconds(60))
            .await
            .expect_err("3rd exceeds quota");
        assert!(matches!(err, ClusterError::QuotaExceeded { .. }));
    }

    #[tokio::test]
    async fn quota_is_scoped_per_owner() {
        let (service, _clock) = service_with_limits(default_limits());
        service
            .create(
                ClientId::new("agent-a"),
                postgres_spec(),
                TimeDelta::seconds(60),
            )
            .await
            .expect("agent-a 1st");
        service
            .create(
                ClientId::new("agent-a"),
                postgres_spec(),
                TimeDelta::seconds(60),
            )
            .await
            .expect("agent-a 2nd");
        service
            .create(
                ClientId::new("agent-b"),
                postgres_spec(),
                TimeDelta::seconds(60),
            )
            .await
            .expect("agent-b unaffected by agent-a's quota");
    }

    #[tokio::test]
    async fn info_returns_not_found_for_unknown_cluster() {
        let (service, _clock) = service_with_limits(default_limits());
        let err = service
            .info(
                &crate::domain::ids::ClusterId::new(ulid::Ulid::nil()),
                &ClientId::new("agent"),
            )
            .await
            .expect_err("unknown");
        assert!(matches!(err, ClusterError::NotFound(_)));
    }

    #[tokio::test]
    async fn info_returns_not_found_for_wrong_owner() {
        let (service, _clock) = service_with_limits(default_limits());
        let cluster = service
            .create(
                ClientId::new("agent-a"),
                postgres_spec(),
                TimeDelta::seconds(60),
            )
            .await
            .expect("create");
        let err = service
            .info(&cluster.id, &ClientId::new("agent-b"))
            .await
            .expect_err("not this owner's cluster");
        assert!(matches!(err, ClusterError::NotFound(_)));
    }

    #[tokio::test]
    async fn info_returns_the_cluster_for_its_owner() {
        let (service, _clock) = service_with_limits(default_limits());
        let owner = ClientId::new("agent");
        let created = service
            .create(owner.clone(), postgres_spec(), TimeDelta::seconds(60))
            .await
            .expect("create");
        let fetched = service.info(&created.id, &owner).await.expect("info");
        assert_eq!(fetched.id, created.id);
    }

    #[tokio::test]
    async fn list_returns_only_the_owners_clusters() {
        let (service, _clock) = service_with_limits(default_limits());
        service
            .create(
                ClientId::new("agent-a"),
                postgres_spec(),
                TimeDelta::seconds(60),
            )
            .await
            .expect("a1");
        service
            .create(
                ClientId::new("agent-b"),
                postgres_spec(),
                TimeDelta::seconds(60),
            )
            .await
            .expect("b1");

        let a_clusters = service
            .list(&ClientId::new("agent-a"))
            .await
            .expect("list a");
        assert_eq!(a_clusters.len(), 1);
    }

    #[tokio::test]
    async fn request_delete_of_a_spawning_cluster_signals_cancel_spawn() {
        let (service, _clock) = service_with_limits(default_limits());
        let owner = ClientId::new("agent");
        let created = service
            .create(owner.clone(), postgres_spec(), TimeDelta::seconds(60))
            .await
            .expect("create");

        let (cluster, outcome) = service
            .request_delete(&created.id, &owner, DeleteReason::UserRequested)
            .await
            .expect("delete");
        assert_eq!(outcome, DeleteOutcome::CancelSpawn);
        assert!(matches!(cluster.state, ClusterState::Deleting { .. }));
    }

    #[tokio::test]
    async fn request_delete_of_a_ready_cluster_signals_start_teardown() {
        let (service, clock) = service_with_limits(default_limits());
        let owner = ClientId::new("agent");
        let created = service
            .create(owner.clone(), postgres_spec(), TimeDelta::seconds(60))
            .await
            .expect("create");

        // Simulate spawn_task having completed successfully before the delete arrives.
        let ready_at = clock.now();
        let connection = crate::domain::service_kind::ConnectionInfo::Postgres(
            crate::domain::service_kind::PostgresConnectionInfo {
                host: "127.0.0.1".to_string(),
                port: 5432,
                dbname: "app_salmon".to_string(),
                user: "app_salmon".to_string(),
                password: crate::redacted::Redacted::new("hunter2".to_string()),
            },
        );
        service
            .repository
            .update_state(
                &created.id,
                &ClusterState::Ready {
                    ready_at,
                    decommission_at: ready_at + TimeDelta::seconds(60),
                    connection,
                },
            )
            .await
            .expect("mark ready");

        let (cluster, outcome) = service
            .request_delete(&created.id, &owner, DeleteReason::UserRequested)
            .await
            .expect("delete");
        assert_eq!(outcome, DeleteOutcome::StartTeardown);
        assert!(matches!(cluster.state, ClusterState::Deleting { .. }));
    }

    #[tokio::test]
    async fn request_delete_is_idempotent_and_reports_already_deleting() {
        let (service, _clock) = service_with_limits(default_limits());
        let owner = ClientId::new("agent");
        let created = service
            .create(owner.clone(), postgres_spec(), TimeDelta::seconds(60))
            .await
            .expect("create");

        service
            .request_delete(&created.id, &owner, DeleteReason::UserRequested)
            .await
            .expect("first delete");
        let (_, second_outcome) = service
            .request_delete(&created.id, &owner, DeleteReason::UserRequested)
            .await
            .expect("second delete is idempotent, not an error");
        assert_eq!(second_outcome, DeleteOutcome::AlreadyDeleting);
    }

    #[tokio::test]
    async fn request_delete_returns_not_found_for_wrong_owner() {
        let (service, _clock) = service_with_limits(default_limits());
        let created = service
            .create(
                ClientId::new("agent-a"),
                postgres_spec(),
                TimeDelta::seconds(60),
            )
            .await
            .expect("create");
        let err = service
            .request_delete(
                &created.id,
                &ClientId::new("agent-b"),
                DeleteReason::UserRequested,
            )
            .await
            .expect_err("not owner");
        assert!(matches!(err, ClusterError::NotFound(_)));
    }

    #[tokio::test]
    async fn a_cluster_mid_deletion_still_counts_against_quota() {
        // DoS-prevention requirement: create+immediately-delete must not free a quota slot for
        // free — the row only actually disappears when teardown_task later calls
        // ClusterRepository::delete, which this service never does on its own.
        let (service, _clock) = service_with_limits(default_limits());
        let owner = ClientId::new("agent");
        let first = service
            .create(owner.clone(), postgres_spec(), TimeDelta::seconds(60))
            .await
            .expect("1st");
        service
            .create(owner.clone(), postgres_spec(), TimeDelta::seconds(60))
            .await
            .expect("2nd");
        service
            .request_delete(&first.id, &owner, DeleteReason::UserRequested)
            .await
            .expect("mark 1st as deleting");

        let err = service
            .create(owner.clone(), postgres_spec(), TimeDelta::seconds(60))
            .await
            .expect_err("still at quota: the deleting row hasn't been removed yet");
        assert!(matches!(err, ClusterError::QuotaExceeded { .. }));
    }
}
