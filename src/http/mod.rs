#[cfg(test)]
mod handler_tests;
pub mod handlers;
pub mod openapi;

use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequestParts};
use axum::http::request::Parts;
use chrono::TimeDelta;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::auth::ClientRegistry;
use crate::domain::ids::ClientId;
use crate::error::ApiError;
use crate::service::cluster_service::ClusterService;
use crate::service::deps::{TaskDeps, TaskRegistry};

/// Everything an HTTP handler needs, cheap to clone (every field is an `Arc`) since axum clones
/// `State` per request.
#[derive(Clone)]
pub struct AppState {
    /// Business-logic orchestration (`create`/`info`/`list`/`request_delete`).
    pub cluster_service: Arc<ClusterService>,
    /// Known clients and their secret hashes, for `Authorization` header verification.
    pub client_registry: Arc<ClientRegistry>,
    /// Dependencies background tasks (spawned by handlers) need — repository, worker pool,
    /// privileged executor, backends, clock.
    pub task_deps: Arc<TaskDeps>,
    /// Cancellation tokens for in-flight spawn tasks, so a `DELETE` on a still-`Spawning` cluster
    /// can signal its background task to stop.
    pub task_registry: Arc<TaskRegistry>,
    /// How long a spawn is expected to take — purely a display hint for `estimated_ready_at`,
    /// not enforced anywhere (the real health-check timeout lives in `PostgresBackend`).
    pub spawn_estimate: TimeDelta,
    /// The request body size cap applied to `POST /clusters` only (via [`DefaultBodyLimit`] in
    /// [`router`]), sized to fit a Supabase `project_tar` upload — every other route keeps axum's
    /// built-in 2MB default.
    pub max_tar_bytes: usize,
}

/// Extracts the authenticated caller from `Authorization: Bearer <name>:<secret>`, rejecting
/// with the same [`ApiError::Auth`] variant regardless of which specific `AuthError` occurred —
/// handlers never need to match on `AuthError` themselves.
pub struct AuthenticatedClient(
    /// The authenticated caller's client ID.
    pub ClientId,
);

impl FromRequestParts<AppState> for AuthenticatedClient {
    /// A failed extraction rejects the request with the same top-level [`ApiError`] every other
    /// handler failure uses, so authentication failures get consistent HTTP-status/body mapping.
    type Rejection = ApiError;

    /// Reads the `Authorization` header from the incoming request and authenticates it against
    /// `state`'s [`ClientRegistry`].
    ///
    /// # Arguments
    ///
    /// - `parts`: the incoming request's head, from which the `Authorization` header is read.
    /// - `state`: the application state holding the `ClientRegistry` to authenticate against.
    ///
    /// # Returns
    ///
    /// An `AuthenticatedClient` wrapping the caller's [`ClientId`].
    ///
    /// # Errors
    ///
    /// [`ApiError::Auth`] if the header is missing, malformed, names an unknown client, or the
    /// secret doesn't match.
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        let client_id = state.client_registry.authenticate(header)?;
        Ok(AuthenticatedClient(client_id))
    }
}

/// Builds the crate's `axum::Router`: Swagger UI + the `OpenAPI` spec mounted at `/`, and the four
/// cluster endpoints, all bound to `state`.
///
/// # Arguments
///
/// - `state`: the application state every handler will receive via axum's `State` extractor.
///
/// # Returns
///
/// A fully configured `Router` ready to be served.
pub fn router(state: AppState) -> Router {
    let max_tar_bytes = state.max_tar_bytes;
    Router::new()
        .merge(SwaggerUi::new("/").url("/openapi.json", openapi::ApiDoc::openapi()))
        .route(
            "/clusters",
            axum::routing::post(handlers::create_cluster)
                .get(handlers::list_clusters)
                .layer(DefaultBodyLimit::max(max_tar_bytes)),
        )
        .route(
            "/clusters/{id}",
            axum::routing::get(handlers::get_cluster).delete(handlers::delete_cluster),
        )
        .with_state(state)
}
