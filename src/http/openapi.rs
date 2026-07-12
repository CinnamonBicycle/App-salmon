//! Machine-readable API spec, generated at compile time from the handler/DTO annotations in
//! `handlers.rs` — satisfies "accessing the root pulls up documentation" and "a machine-readable
//! spec" with one mechanism (`utoipa` + `utoipa-swagger-ui`, mounted in `http::router`).

use utoipa::OpenApi;

use crate::http::handlers;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "App Salmon",
        description = "Ephemeral Postgres+pgvector cluster provisioning for integration tests.",
        version = "0.1.0"
    ),
    paths(
        handlers::create_cluster,
        handlers::get_cluster,
        handlers::list_clusters,
        handlers::delete_cluster
    ),
    components(schemas(
        handlers::CreateClusterRequest,
        handlers::CreateClusterResponse,
        handlers::ClusterInfoResponse,
        handlers::ConnectionResponse,
        handlers::ClusterListEntry,
        handlers::DeleteResponse,
    ))
)]
pub struct ApiDoc;
