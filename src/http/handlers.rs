//! The 4 endpoints. Every handler returns `Result<_, ApiError>`; `ApiError`'s `IntoResponse` impl
//! (see `error.rs`) is the only place HTTP status codes get decided for genuine failures. The
//! "known state" responses (spawning/ready/failed/deleting) are built directly here instead,
//! since they're not errors — a `Deleting` cluster reported via `410 Gone` is a normal, expected
//! outcome of a successful lookup, not a failure this handler experienced.

use axum::Json;
use axum::extract::{FromRequest, Multipart, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::cluster::{Cluster, ClusterState, DeleteReason};
use crate::domain::ids::ClusterId;
use crate::domain::service_kind::{ConnectionInfo, ServiceKind, ServiceSpec};
use crate::error::ApiError;
use crate::http::{AppState, AuthenticatedClient};
use crate::service::cluster_service::DeleteOutcome;
use crate::service::{spawn_task, teardown_task};

/// The `POST /clusters` request body. For `ServiceKind::Postgres`, sent directly as a JSON
/// request body. For `ServiceKind::Supabase`, sent as the `metadata` part of a
/// `multipart/form-data` request alongside a `project_tar` part (see [`create_cluster`]) — the
/// shape is identical either way.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateClusterRequest {
    /// Which kind of service to provision. `Supabase` requires the `multipart/form-data` path;
    /// `Postgres` requires the plain-JSON path (see [`create_cluster`]).
    pub service: ServiceKind,
    /// Whether to enable the `pgvector` extension once the database is ready. Defaults to
    /// `false` if omitted. Ignored for `ServiceKind::Supabase`, which always enables it.
    #[serde(default)]
    pub pgvector: bool,
    /// Requested time-to-live in seconds, anchored to when the cluster becomes ready (not when
    /// this request is made) — must fall within the server's configured min/max TTL bounds.
    pub ttl_secs: i64,
}

/// The `202 Accepted` response body for `POST /clusters`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CreateClusterResponse {
    /// The new cluster's ID, to be used in subsequent `GET`/`DELETE` calls.
    pub id: String,
    /// Always `"spawning"` at this point — the cluster has just been accepted, not yet ready.
    pub status: &'static str,
    /// When the create request was received.
    pub requested_at: DateTime<Utc>,
    /// A display-only estimate of when the cluster will become ready; not a guarantee.
    pub estimated_ready_at: DateTime<Utc>,
}

/// Postgres connection details, standalone for a `ServiceKind::Postgres` cluster or nested inside
/// [`ConnectionResponse::Supabase`] for the underlying Postgres instance of a
/// `ServiceKind::Supabase` cluster.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PostgresConnectionResponse {
    /// Host to connect to (always `127.0.0.1`, since App Salmon and its clusters share a host).
    pub host: String,
    /// Port to connect to.
    pub port: u16,
    /// Database name.
    pub dbname: String,
    /// Database user.
    pub user: String,
    /// Database password, in plaintext (this endpoint is the one place it's ever exposed).
    pub password: String,
}

/// Connection details for a `Ready` cluster — which variant is returned matches the cluster's
/// `ServiceKind`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConnectionResponse {
    /// Connection details for a `ServiceKind::Postgres` cluster.
    Postgres {
        /// Host to connect to.
        host: String,
        /// Port to connect to.
        port: u16,
        /// Database name.
        dbname: String,
        /// Database user.
        user: String,
        /// Database password, in plaintext.
        password: String,
    },
    /// Connection details for a `ServiceKind::Supabase` cluster.
    Supabase {
        /// Kong's published `host:port` — the single ingress for API/auth/edge-function traffic.
        api_url: String,
        /// Direct connection details for the underlying Postgres instance.
        postgres: PostgresConnectionResponse,
        /// A JWT signed with the `anon` role, in plaintext.
        anon_key: String,
        /// A JWT signed with the `service_role` role, in plaintext.
        service_role_key: String,
        /// The secret `anon_key`/`service_role_key` are signed with, in plaintext.
        jwt_secret: String,
    },
}

/// The body of `GET /clusters/{id}` (and each entry of `GET /clusters`) — which variant is
/// returned reflects the cluster's current lifecycle state.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ClusterInfoResponse {
    /// The cluster has not yet become ready or failed.
    Spawning {
        /// When the cluster was originally requested.
        requested_at: DateTime<Utc>,
        /// A display-only estimate of when it'll become ready.
        estimated_ready_at: DateTime<Utc>,
    },
    /// The cluster is ready to connect to.
    Ready {
        /// When the cluster was originally requested.
        requested_at: DateTime<Utc>,
        /// When the cluster actually became ready — the anchor for its TTL.
        started_at: DateTime<Utc>,
        /// When the cluster is scheduled to be automatically torn down (`started_at` + the
        /// requested TTL).
        scheduled_decommission_at: DateTime<Utc>,
        /// How to connect to it.
        connection: ConnectionResponse,
    },
    /// The cluster failed to spawn and will not become ready.
    Failed {
        /// When the cluster was originally requested.
        requested_at: DateTime<Utc>,
        /// When the failure was recorded.
        failed_at: DateTime<Utc>,
        /// A sanitized, non-sensitive summary of what went wrong.
        error: String,
    },
    /// The cluster is being torn down (from any prior state) and will disappear entirely once
    /// teardown finishes.
    Deleting {
        /// A fixed, human-readable status message.
        message: &'static str,
    },
}

/// One entry in the `GET /clusters` response body — a cluster's ID plus its current info.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ClusterListEntry {
    /// The cluster's ID.
    pub id: String,
    /// The cluster's current state, flattened into the same JSON object as `id`.
    #[serde(flatten)]
    pub info: ClusterInfoResponse,
}

/// The `202 Accepted` response body for `DELETE /clusters/{id}`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DeleteResponse {
    /// Always `"deleting"` — deletion has been accepted (or was already in progress).
    pub status: &'static str,
}

/// Maps a domain [`ConnectionInfo`] onto the wire [`ConnectionResponse`] shape, exposing every
/// secret as plaintext — this response is the one place that's meant to happen (matching
/// [`PostgresConnectionResponse::password`]'s existing doc comment).
///
/// # Arguments
///
/// - `connection`: the connection info to translate.
///
/// # Returns
///
/// The matching [`ConnectionResponse`] variant.
fn connection_response(connection: &ConnectionInfo) -> ConnectionResponse {
    match connection {
        ConnectionInfo::Postgres(postgres) => ConnectionResponse::Postgres {
            host: postgres.host.clone(),
            port: postgres.port,
            dbname: postgres.dbname.clone(),
            user: postgres.user.clone(),
            password: postgres.password.expose().clone(),
        },
        ConnectionInfo::Supabase(supabase) => ConnectionResponse::Supabase {
            api_url: supabase.api_url.clone(),
            postgres: PostgresConnectionResponse {
                host: supabase.postgres.host.clone(),
                port: supabase.postgres.port,
                dbname: supabase.postgres.dbname.clone(),
                user: supabase.postgres.user.clone(),
                password: supabase.postgres.password.expose().clone(),
            },
            anon_key: supabase.anon_key.expose().clone(),
            service_role_key: supabase.service_role_key.expose().clone(),
            jwt_secret: supabase.jwt_secret.expose().clone(),
        },
    }
}

/// Maps a cluster's current lifecycle state to the HTTP status code and response body `GET`
/// endpoints should return for it.
///
/// # Arguments
///
/// - `cluster`: the cluster to describe.
/// - `spawn_estimate`: the display-only spawn-duration estimate used to compute
///   `estimated_ready_at` for a still-`Spawning` cluster.
///
/// # Returns
///
/// The HTTP status (`200` for `Spawning`/`Ready`/`Failed`, `410 Gone` for `Deleting`) paired with
/// the response body to serialize.
fn info_response(
    cluster: &Cluster,
    spawn_estimate: TimeDelta,
) -> (StatusCode, ClusterInfoResponse) {
    match &cluster.state {
        ClusterState::Spawning { started_at } => (
            StatusCode::OK,
            ClusterInfoResponse::Spawning {
                requested_at: cluster.requested_at,
                estimated_ready_at: *started_at + spawn_estimate,
            },
        ),
        ClusterState::Ready {
            ready_at,
            decommission_at,
            connection,
        } => (
            StatusCode::OK,
            ClusterInfoResponse::Ready {
                requested_at: cluster.requested_at,
                started_at: *ready_at,
                scheduled_decommission_at: *decommission_at,
                connection: connection_response(connection),
            },
        ),
        ClusterState::Failed {
            failed_at,
            error_summary,
        } => (
            StatusCode::OK,
            ClusterInfoResponse::Failed {
                requested_at: cluster.requested_at,
                failed_at: *failed_at,
                error: error_summary.clone(),
            },
        ),
        ClusterState::Deleting { .. } => (
            StatusCode::GONE,
            ClusterInfoResponse::Deleting {
                message: "cluster is being torn down",
            },
        ),
    }
}

/// Spawns the background task that actually provisions `cluster`, registering its cancellation
/// token first so a `DELETE` arriving before the task finishes can signal it — and unregistering
/// that token once the task returns, however it finished.
///
/// # Arguments
///
/// - `state`: the application state to pull `task_deps`/`task_registry` from.
/// - `cluster`: the newly created cluster (still in `Spawning` state) to provision.
/// - `project_tar`: the raw bytes of a caller-uploaded project tree, if any — see
///   [`crate::service::spawn_task::run`].
fn launch_spawn(state: &AppState, cluster: Cluster, project_tar: Option<Vec<u8>>) {
    let cluster_id = cluster.id;
    let cancel = state.task_registry.register(cluster_id);
    let deps = state.task_deps.clone();
    let registry = state.task_registry.clone();
    tokio::spawn(async move {
        spawn_task::run(deps, cluster, cancel, project_tar).await;
        registry.unregister(&cluster_id);
    });
}

/// Reads a `multipart/form-data` `POST /clusters` request: a `metadata` part (JSON, same shape
/// as [`CreateClusterRequest`]) and a `project_tar` part (raw bytes). Both parts are read fully
/// so oversized uploads are rejected here — via the `max_tar_bytes`-sized `DefaultBodyLimit`
/// layer on this route (see `http::router`), which surfaces as a multipart read error mid-stream
/// — rather than passed further into the system.
///
/// This layer only enforces the wire-size cap; it never inspects `project_tar`'s structure —
/// that validation happens exactly once, downstream, at the point that actually has a
/// destination to extract into (`service::spawn_task::adopt_project_tar`, called from
/// `do_spawn` once the cluster's worker/slot directory exists — see `docs/DESIGN.md` §11 for why).
///
/// # Arguments
///
/// - `multipart`: the incoming multipart body to read.
///
/// # Returns
///
/// The parsed `metadata` part, once both parts have been read and `service` is confirmed to be
/// [`ServiceKind::Supabase`] (the only kind this path accepts), together with `project_tar`'s raw
/// bytes.
///
/// # Errors
///
/// [`ApiError::BadRequest`] if a part is missing, the `metadata` part isn't valid JSON matching
/// [`CreateClusterRequest`], `service` isn't `Supabase`, or the multipart body itself is
/// malformed or exceeds the configured size limit.
async fn parse_multipart_create_request(
    mut multipart: Multipart,
) -> Result<(CreateClusterRequest, Vec<u8>), ApiError> {
    let mut metadata: Option<CreateClusterRequest> = None;
    let mut project_tar: Option<Vec<u8>> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| ApiError::BadRequest(format!("invalid multipart body: {err}")))?
    {
        match field.name() {
            Some("metadata") => {
                let bytes = field.bytes().await.map_err(|err| {
                    ApiError::BadRequest(format!("invalid multipart body: {err}"))
                })?;
                metadata = Some(serde_json::from_slice(&bytes).map_err(|err| {
                    ApiError::BadRequest(format!("invalid \"metadata\" part: {err}"))
                })?);
            }
            Some("project_tar") => {
                let bytes = field.bytes().await.map_err(|err| {
                    ApiError::BadRequest(format!("invalid multipart body: {err}"))
                })?;
                project_tar = Some(bytes.to_vec());
            }
            _ => {}
        }
    }

    let metadata = metadata.ok_or_else(|| {
        ApiError::BadRequest("multipart request missing a \"metadata\" part".to_string())
    })?;
    let project_tar = project_tar.ok_or_else(|| {
        ApiError::BadRequest("multipart request missing a \"project_tar\" part".to_string())
    })?;
    if metadata.service != ServiceKind::Supabase {
        return Err(ApiError::BadRequest(
            "multipart/form-data requests are only accepted for service \"supabase\"".to_string(),
        ));
    }

    Ok((metadata, project_tar))
}

/// Validates and accepts a new cluster request, then launches its background provisioning task.
///
/// Branches on `Content-Type`: `application/json` is parsed directly as [`CreateClusterRequest`]
/// and only accepts `ServiceKind::Postgres`; `multipart/form-data` is parsed via
/// [`parse_multipart_create_request`] (a `metadata` JSON part plus a `project_tar` bytes part)
/// and only accepts `ServiceKind::Supabase`. Either combination the wrong way round is rejected
/// with `400`.
///
/// # Arguments
///
/// - `state`: the application state (extracted via axum's `State`), providing the cluster
///   service and background-task dependencies.
/// - `owner`: the authenticated caller (extracted via [`AuthenticatedClient`]), who will own the
///   new cluster.
/// - `request`: the raw incoming request, so this handler can branch on `Content-Type` before
///   choosing how to parse the body.
///
/// # Returns
///
/// `202 Accepted` with a [`CreateClusterResponse`] once the request has been validated and
/// accepted — provisioning itself continues in the background; poll `GET /clusters/{id}` for
/// completion.
///
/// # Errors
///
/// [`ApiError::BadRequest`] for an out-of-range `ttl_secs`, a malformed body, or a
/// `Content-Type`/`service` mismatch, plus whatever
/// [`crate::service::cluster_service::ClusterService::create`] returns for TTL/quota validation
/// or storage failures.
#[utoipa::path(
    post,
    path = "/clusters",
    request_body = CreateClusterRequest,
    responses(
        (status = 202, description = "Cluster creation accepted; poll GET /clusters/{id}", body = CreateClusterResponse),
        (status = 400, description = "TTL out of bounds, unsupported service kind, or Content-Type/service mismatch"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 429, description = "Owner is already at their cluster quota"),
        (status = 503, description = "Worker pool exhausted or Docker daemon unreachable"),
    )
)]
pub async fn create_cluster(
    State(state): State<AppState>,
    AuthenticatedClient(owner): AuthenticatedClient,
    request: Request,
) -> Result<Response, ApiError> {
    let is_multipart = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("multipart/form-data"));

    let (request, project_tar) = if is_multipart {
        let multipart = Multipart::from_request(request, &state)
            .await
            .map_err(|err| ApiError::BadRequest(format!("invalid multipart body: {err}")))?;
        let (request, project_tar) = parse_multipart_create_request(multipart).await?;
        (request, Some(project_tar))
    } else {
        let Json(body) = Json::<CreateClusterRequest>::from_request(request, &state)
            .await
            .map_err(|err| ApiError::BadRequest(format!("invalid JSON body: {err}")))?;
        if body.service != ServiceKind::Postgres {
            return Err(ApiError::BadRequest(
                "service \"supabase\" requires a multipart/form-data request carrying a \
                 project_tar part"
                    .to_string(),
            ));
        }
        (body, None)
    };

    let ttl = TimeDelta::try_seconds(request.ttl_secs)
        .ok_or_else(|| ApiError::BadRequest("ttl_secs out of range".to_string()))?;
    let service = ServiceSpec {
        kind: request.service,
        pgvector: request.pgvector,
    };

    let cluster = state.cluster_service.create(owner, service, ttl).await?;
    let requested_at = cluster.requested_at;
    let estimated_ready_at = requested_at + state.spawn_estimate;
    let id = cluster.id.to_string();

    launch_spawn(&state, cluster, project_tar);

    let body = CreateClusterResponse {
        id,
        status: "spawning",
        requested_at,
        estimated_ready_at,
    };
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

/// Looks up a single cluster's current state.
///
/// # Arguments
///
/// - `state`: the application state (extracted via axum's `State`).
/// - `owner`: the authenticated caller (extracted via [`AuthenticatedClient`]) — the cluster must
///   belong to them, or it's reported as not found.
/// - `id`: the cluster ID to look up (extracted from the URL path).
///
/// # Returns
///
/// `200 OK` with a [`ClusterInfoResponse`] if the cluster is `Spawning`, `Ready`, or `Failed`;
/// `410 Gone` (also with a body) if it's `Deleting`.
///
/// # Errors
///
/// [`crate::error::ApiError::NotFound`] if the cluster never existed or isn't owned by the
/// caller.
#[utoipa::path(
    get,
    path = "/clusters/{id}",
    responses(
        (status = 200, description = "Cluster is spawning, ready, or failed", body = ClusterInfoResponse),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 404, description = "Cluster never existed, isn't yours, or has finished being deleted"),
        (status = 410, description = "Cluster is being torn down"),
    )
)]
pub async fn get_cluster(
    State(state): State<AppState>,
    AuthenticatedClient(owner): AuthenticatedClient,
    Path(id): Path<ClusterId>,
) -> Result<Response, ApiError> {
    let cluster = state.cluster_service.info(&id, &owner).await?;
    let (status, body) = info_response(&cluster, state.spawn_estimate);
    Ok((status, Json(body)).into_response())
}

/// Lists every cluster owned by the authenticated caller, in any non-deleted state.
///
/// # Arguments
///
/// - `state`: the application state (extracted via axum's `State`).
/// - `owner`: the authenticated caller (extracted via [`AuthenticatedClient`]) whose clusters to
///   list.
///
/// # Returns
///
/// `200 OK` with a JSON array of [`ClusterListEntry`], one per owned cluster.
///
/// # Errors
///
/// A wrapped [`crate::ports::repository::RepositoryError`] on a storage failure.
#[utoipa::path(
    get,
    path = "/clusters",
    responses(
        (status = 200, description = "All of the caller's clusters, in any non-deleted state", body = [ClusterListEntry]),
        (status = 401, description = "Missing or invalid credentials"),
    )
)]
pub async fn list_clusters(
    State(state): State<AppState>,
    AuthenticatedClient(owner): AuthenticatedClient,
) -> Result<Response, ApiError> {
    let clusters = state.cluster_service.list(&owner).await?;
    let entries: Vec<ClusterListEntry> = clusters
        .iter()
        .map(|cluster| {
            let (_, info) = info_response(cluster, state.spawn_estimate);
            ClusterListEntry {
                id: cluster.id.to_string(),
                info,
            }
        })
        .collect();
    Ok((StatusCode::OK, Json(entries)).into_response())
}

/// Requests deletion of a cluster — idempotent if it's already being deleted. Depending on what
/// state the cluster was in, this either signals its in-flight spawn task to cancel or spawns a
/// fresh teardown task; see [`DeleteOutcome`].
///
/// # Arguments
///
/// - `state`: the application state (extracted via axum's `State`), providing the cluster
///   service, task deps, and task registry.
/// - `owner`: the authenticated caller (extracted via [`AuthenticatedClient`]) — the cluster must
///   belong to them, or it's reported as not found.
/// - `id`: the cluster ID to delete (extracted from the URL path).
///
/// # Returns
///
/// `202 Accepted` with a [`DeleteResponse`] once deletion has been accepted (or was already in
/// progress) — teardown itself continues in the background.
///
/// # Errors
///
/// [`crate::error::ApiError::NotFound`] if the cluster never existed or isn't owned by the
/// caller.
#[utoipa::path(
    delete,
    path = "/clusters/{id}",
    responses(
        (status = 202, description = "Deletion accepted (or already in progress)", body = DeleteResponse),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 404, description = "Cluster never existed or isn't yours"),
    )
)]
pub async fn delete_cluster(
    state: State<AppState>,
    AuthenticatedClient(owner): AuthenticatedClient,
    Path(id): Path<ClusterId>,
) -> Result<Response, ApiError> {
    let (cluster, outcome) = state
        .cluster_service
        .request_delete(&id, &owner, DeleteReason::UserRequested)
        .await?;

    match outcome {
        DeleteOutcome::CancelSpawn => state.task_registry.cancel(&id),
        DeleteOutcome::StartTeardown => {
            tokio::spawn(teardown_task::run(state.task_deps.clone(), cluster));
        }
        DeleteOutcome::AlreadyDeleting => {}
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(DeleteResponse { status: "deleting" }),
    )
        .into_response())
}
