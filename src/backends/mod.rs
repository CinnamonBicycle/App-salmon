//! The set of pluggable cluster backends. Phase 1 registers only [`postgres::PostgresBackend`];
//! adding a `ServiceKind` variant later (Supabase, an `OpenRouter` proxy) means adding a matching
//! `ClusterBackend` impl and registering it — nothing else in the request path changes, because
//! `ClusterService::create` looks the backend up by kind and rejects the request (400) if none
//! is registered for it.

pub mod postgres;

use async_trait::async_trait;

use crate::domain::cluster::ClusterError;
use crate::domain::ids::{ClusterId, WorkerUser};
use crate::domain::service_kind::{ConnectionInfo, ServiceKind, ServiceSpec};

#[async_trait]
pub trait ClusterBackend: Send + Sync {
    /// Which [`ServiceKind`] this backend implements — used by `ClusterService::create` to look
    /// up the right backend for a request, and by callers that need to know without holding a
    /// `ServiceSpec` in hand.
    ///
    /// # Returns
    ///
    /// The single `ServiceKind` this backend instance handles.
    fn kind(&self) -> ServiceKind;

    /// Creates and starts whatever this backend needs for `cluster_id`, running as `worker`
    /// (both for the container's `--user` and for the on-disk directory it's bind-mounted into),
    /// and returns how to connect to it once ready. Does not allocate or prepare `worker` itself
    /// — the caller (`service::spawn_task`) owns that, so the backend stays focused on "what
    /// container(s) does this service kind need."
    ///
    /// # Arguments
    ///
    /// - `cluster_id`: the cluster this resource is being provisioned for — used to derive a
    ///   deterministic, recomputable resource name/identity.
    /// - `worker`: the pre-allocated worker account this backend's resources must run/be owned
    ///   as, both for process privilege-drop and for on-disk attribution.
    /// - `service`: the caller's requested service configuration (kind plus any kind-specific
    ///   options, e.g. whether to enable `pgvector`).
    ///
    /// # Returns
    ///
    /// A [`ConnectionInfo`] a client can use to connect to the now-ready resource.
    ///
    /// # Errors
    ///
    /// A [`ClusterError`] if the underlying resource fails to create, never becomes ready within
    /// its backend-specific timeout, or (where applicable) fails a post-readiness setup step.
    async fn spawn(
        &self,
        cluster_id: &ClusterId,
        worker: &WorkerUser,
        service: &ServiceSpec,
    ) -> Result<ConnectionInfo, ClusterError>;

    /// Stops and removes whatever `spawn` created for `cluster_id`. Idempotent: tolerates being
    /// called against a cluster whose container is already gone (e.g. resuming after a crash).
    ///
    /// # Arguments
    ///
    /// - `cluster_id`: the cluster whose resources should be torn down.
    ///
    /// # Returns
    ///
    /// Nothing, on success.
    ///
    /// # Errors
    ///
    /// A [`ClusterError`] if the underlying teardown call itself fails (not raised merely because
    /// the resource was already gone — that's the idempotent success case).
    async fn teardown(&self, cluster_id: &ClusterId) -> Result<(), ClusterError>;

    /// Whether `cluster_id`'s underlying resources still exist and are running. Used only by
    /// `service::reconciliation` at startup, to detect a `Ready` cluster whose container didn't
    /// survive a restart — kept on the backend (rather than reconciliation querying
    /// `ContainerRuntime` directly) so container-naming/identity details stay encapsulated here.
    ///
    /// # Arguments
    ///
    /// - `cluster_id`: the cluster whose resources should be checked.
    ///
    /// # Returns
    ///
    /// `true` if the resource still exists and is running, `false` if it's gone or stopped.
    ///
    /// # Errors
    ///
    /// A [`ClusterError`] if the liveness check itself couldn't be completed (as distinct from a
    /// successful check that finds the resource absent, which is `Ok(false)`).
    async fn is_alive(&self, cluster_id: &ClusterId) -> Result<bool, ClusterError>;
}
