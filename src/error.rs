//! `ApiError` is the only type in this crate with an `IntoResponse` impl. Every HTTP handler
//! returns `Result<T, ApiError>`; lower-layer errors ([`ClusterError`] and friends) convert into
//! it via a hand-written `From` (not `#[from]`, since the mapping branches on lower-layer
//! variants) so handlers never match on `bollard`/`rusqlite`/sudo-specific error types directly.
//!
//! Deliberately does not log (or otherwise expose) the `Display`/`Debug` text of the wrapped
//! internal error here: a `DockerError`'s source may echo back request content (e.g. if the
//! daemon includes the submitted container spec — which carries the generated DB password — in
//! a 4xx error body). Call sites closer to where an error actually occurs are responsible for
//! logging sanitized detail; this layer only logs which *category* of internal error occurred.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;

use crate::auth::AuthError;
use crate::domain::cluster::ClusterError;
use crate::ports::container_runtime::DockerError;
use crate::ports::repository::RepositoryError;
use crate::worker_pool::WorkerPoolError;

/// Every internal (non-auth, non-domain-validation) failure that can reach [`ApiError::Internal`]
/// — always mapped to a `500`, with only [`InternalError::category`]'s coarse name logged, never
/// the wrapped error's own `Display` text (see module docs for why).
#[derive(Debug, Error)]
pub enum InternalError {
    /// A durable-storage failure (`SQLite`, (de)serialization, corrupt row).
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    /// A worker-pool failure other than plain exhaustion (which maps to
    /// [`ApiError::Unavailable`] instead — see [`From<ClusterError>`] below).
    #[error(transparent)]
    WorkerPool(#[from] WorkerPoolError),
    /// A Docker/`bollard` failure other than a daemon-unreachable `Connect` error (which maps to
    /// [`ApiError::Unavailable`] instead — see [`From<ClusterError>`] below).
    #[error(transparent)]
    Docker(#[from] DockerError),
    /// See [`crate::domain::cluster::ClusterError::BackendSpawnFailed`]. Only reaches `ApiError`
    /// defensively — in normal operation a spawn failure is observed and persisted by the
    /// background spawn task, never propagated through an HTTP handler.
    #[error("backend error: {0}")]
    Backend(String),
}

impl InternalError {
    /// A category name safe to log or return — never the wrapped error's `Display` text.
    ///
    /// # Returns
    ///
    /// A short, static, non-secret label identifying which `InternalError` variant `self` is
    /// (`"repository"`, `"worker_pool"`, `"docker"`, or `"backend"`), suitable for a log field.
    fn category(&self) -> &'static str {
        match self {
            InternalError::Repository(_) => "repository",
            InternalError::WorkerPool(_) => "worker_pool",
            InternalError::Docker(_) => "docker",
            InternalError::Backend(_) => "backend",
        }
    }
}

/// The top-level, HTTP-facing error type — the only type in this crate with an [`IntoResponse`]
/// impl. Every variant maps to exactly one HTTP status code; see [`ApiError::into_response`].
#[derive(Debug, Error)]
pub enum ApiError {
    /// Missing/malformed/invalid credentials — maps to `401 Unauthorized`.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// The request itself was invalid (e.g. a TTL out of bounds) — maps to `400 Bad Request`. The
    /// string is the caller-facing explanation.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// No cluster exists at the requested id for this caller (never existed, belongs to someone
    /// else, or has been deleted — all indistinguishable by design) — maps to `404 Not Found`.
    #[error("cluster not found")]
    NotFound,
    /// The caller already holds the maximum number of clusters — maps to
    /// `429 Too Many Requests`.
    #[error("quota exceeded")]
    QuotaExceeded,
    /// A dependency the request needs is temporarily down (worker pool exhausted, Docker daemon
    /// unreachable) — maps to `503 Service Unavailable`. The string is the caller-facing
    /// explanation.
    #[error("temporarily unavailable: {0}")]
    Unavailable(String),
    /// An internal failure not attributable to caller input or a known-transient dependency
    /// outage — maps to `500 Internal Server Error`, with detail withheld from the response body.
    #[error(transparent)]
    Internal(#[from] InternalError),
}

impl From<ClusterError> for ApiError {
    /// Maps a domain-layer [`ClusterError`] to the [`ApiError`] (and so HTTP status) it should
    /// produce. A hand-written `From` (not `#[from]`) since the mapping branches on the wrapped
    /// variant rather than being a 1:1 wrap.
    ///
    /// # Arguments
    ///
    /// - `err`: the domain error to convert.
    ///
    /// # Returns
    ///
    /// The corresponding `ApiError`: `BadRequest` for TTL/invalid-transition errors,
    /// `QuotaExceeded`/`NotFound` passed through directly, `Unavailable` for pool exhaustion or a
    /// Docker daemon connect failure specifically, and `Internal` (wrapping the appropriate
    /// [`InternalError`] variant) for everything else.
    fn from(err: ClusterError) -> Self {
        match err {
            ClusterError::TtlOutOfBounds { .. } | ClusterError::InvalidTransition { .. } => {
                ApiError::BadRequest(err.to_string())
            }
            ClusterError::QuotaExceeded { .. } => ApiError::QuotaExceeded,
            ClusterError::NotFound(_) => ApiError::NotFound,
            ClusterError::WorkerPool(WorkerPoolError::PoolExhausted { .. }) => {
                ApiError::Unavailable("worker pool exhausted".to_string())
            }
            ClusterError::WorkerPool(other) => ApiError::Internal(InternalError::WorkerPool(other)),
            ClusterError::Docker(DockerError::Connect { .. }) => {
                ApiError::Unavailable("docker daemon unreachable".to_string())
            }
            ClusterError::Docker(other) => ApiError::Internal(InternalError::Docker(other)),
            ClusterError::Repository(other) => ApiError::Internal(InternalError::Repository(other)),
            ClusterError::BackendSpawnFailed(message) => {
                ApiError::Internal(InternalError::Backend(message))
            }
        }
    }
}

/// The JSON shape every error response body takes.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct ErrorBody {
    /// A short, stable, machine-matchable error code (e.g. `"not_found"`, `"quota_exceeded"`).
    error: &'static str,
    /// A human-readable explanation. Never includes raw internal error text — see module docs.
    message: String,
}

impl IntoResponse for ApiError {
    /// Converts this error into the HTTP response it produces: the status code documented on each
    /// [`ApiError`] variant, plus a JSON [`ErrorBody`]. For [`ApiError::Internal`], also logs the
    /// error's category (never its `Display` text) at `error` level before responding.
    ///
    /// # Returns
    ///
    /// The `axum` [`Response`] representing this error.
    fn into_response(self) -> Response {
        let (status, error, message) = match &self {
            ApiError::Auth(_) => (StatusCode::UNAUTHORIZED, "unauthorized", self.to_string()),
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request", self.to_string()),
            ApiError::NotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "cluster may never have existed or may have been deleted".to_string(),
            ),
            ApiError::QuotaExceeded => (
                StatusCode::TOO_MANY_REQUESTS,
                "quota_exceeded",
                self.to_string(),
            ),
            ApiError::Unavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                self.to_string(),
            ),
            ApiError::Internal(internal) => {
                tracing::error!(
                    category = internal.category(),
                    "returning 500 for internal error"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "internal error".to_string(),
                )
            }
        };
        (status, Json(ErrorBody { error, message })).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiError, InternalError};
    use crate::auth::AuthError;
    use crate::domain::cluster::ClusterError;
    use crate::domain::ids::{ClientId, ClusterId};
    use crate::ports::container_runtime::DockerError;
    use crate::worker_pool::WorkerPoolError;
    use axum::response::IntoResponse;
    use bollard::errors::Error as BollardError;

    fn status_of(err: ApiError) -> axum::http::StatusCode {
        err.into_response().status()
    }

    #[test]
    fn auth_error_maps_to_401() {
        assert_eq!(
            status_of(ApiError::from(AuthError::MissingCredentials)),
            axum::http::StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn bad_request_maps_to_400() {
        assert_eq!(
            status_of(ApiError::BadRequest("bad".to_string())),
            axum::http::StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn not_found_maps_to_404() {
        assert_eq!(
            status_of(ApiError::NotFound),
            axum::http::StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn quota_exceeded_maps_to_429() {
        assert_eq!(
            status_of(ApiError::QuotaExceeded),
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[test]
    fn unavailable_maps_to_503() {
        assert_eq!(
            status_of(ApiError::Unavailable("down".to_string())),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn internal_maps_to_500() {
        assert_eq!(
            status_of(ApiError::Internal(InternalError::Repository(
                crate::ports::repository::RepositoryError::Migration("boom".to_string())
            ))),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn internal_error_response_never_includes_raw_source_text() {
        let response =
            ApiError::Internal(InternalError::WorkerPool(WorkerPoolError::PoolExhausted {
                pool_size: 3,
            }))
            .into_response();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn cluster_error_ttl_out_of_bounds_maps_to_bad_request() {
        let err: ApiError = ClusterError::TtlOutOfBounds {
            requested_secs: 5,
            min_secs: 30,
            max_secs: 3600,
        }
        .into();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn cluster_error_quota_exceeded_maps_through() {
        let err: ApiError = ClusterError::QuotaExceeded {
            owner: ClientId::new("agent"),
            count: 2,
            limit: 2,
        }
        .into();
        assert!(matches!(err, ApiError::QuotaExceeded));
    }

    #[test]
    fn cluster_error_not_found_maps_through() {
        let err: ApiError = ClusterError::NotFound(ClusterId::new(ulid::Ulid::nil())).into();
        assert!(matches!(err, ApiError::NotFound));
    }

    #[test]
    fn cluster_error_pool_exhausted_maps_to_unavailable() {
        let err: ApiError =
            ClusterError::WorkerPool(WorkerPoolError::PoolExhausted { pool_size: 3 }).into();
        assert!(matches!(err, ApiError::Unavailable(_)));
    }

    #[test]
    fn cluster_error_other_worker_pool_errors_map_to_internal() {
        let err: ApiError = ClusterError::WorkerPool(WorkerPoolError::DoubleRelease {
            worker: crate::domain::ids::WorkerUser::new("salmon-worker-00", 2000, 2000),
        })
        .into();
        assert!(matches!(
            err,
            ApiError::Internal(InternalError::WorkerPool(_))
        ));
    }

    #[test]
    fn cluster_error_docker_connect_maps_to_unavailable() {
        let err: ApiError = ClusterError::Docker(DockerError::Connect {
            socket: "/var/run/docker.sock".to_string(),
            source: BollardError::DockerResponseServerError {
                status_code: 500,
                message: "boom".to_string(),
            },
        })
        .into();
        assert!(matches!(err, ApiError::Unavailable(_)));
    }

    #[test]
    fn cluster_error_other_docker_errors_map_to_internal() {
        let err: ApiError = ClusterError::Docker(DockerError::HealthCheckTimeout {
            container: crate::ports::container_runtime::ContainerHandle::new("abc123"),
            waited_secs: 60,
        })
        .into();
        assert!(matches!(err, ApiError::Internal(InternalError::Docker(_))));
    }

    #[test]
    fn cluster_error_repository_errors_map_to_internal() {
        let err: ApiError = ClusterError::Repository(
            crate::ports::repository::RepositoryError::Migration("boom".to_string()),
        )
        .into();
        assert!(matches!(
            err,
            ApiError::Internal(InternalError::Repository(_))
        ));
    }

    #[test]
    fn cluster_error_backend_spawn_failed_maps_to_internal() {
        let err: ApiError =
            ClusterError::BackendSpawnFailed("pgvector setup failed".to_string()).into();
        assert!(matches!(err, ApiError::Internal(InternalError::Backend(_))));
    }

    #[test]
    fn internal_docker_variant_maps_to_500_through_into_response() {
        assert_eq!(
            status_of(ApiError::Internal(InternalError::Docker(
                DockerError::HealthCheckTimeout {
                    container: crate::ports::container_runtime::ContainerHandle::new("abc123"),
                    waited_secs: 60,
                }
            ))),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn internal_backend_variant_maps_to_500_through_into_response() {
        assert_eq!(
            status_of(ApiError::Internal(InternalError::Backend(
                "boom".to_string()
            ))),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// `InternalError::category()` is only called from inside a `tracing::error!` field
    /// expression, and `tracing` skips evaluating a field expression entirely when no subscriber
    /// is listening at that level — which is the case in every other test in this module (no
    /// global subscriber is installed for unit tests). Without a subscriber active, `category()`
    /// is provably never called, not merely untested; this test installs one for the duration of
    /// the call so the field expression actually runs, over every `InternalError` variant.
    #[test]
    fn internal_error_category_is_evaluated_for_every_variant_when_a_subscriber_is_listening() {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .with_max_level(tracing::Level::ERROR)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let variants = [
                InternalError::Repository(crate::ports::repository::RepositoryError::Migration(
                    "boom".to_string(),
                )),
                InternalError::WorkerPool(WorkerPoolError::PoolExhausted { pool_size: 1 }),
                InternalError::Docker(DockerError::HealthCheckTimeout {
                    container: crate::ports::container_runtime::ContainerHandle::new("abc123"),
                    waited_secs: 1,
                }),
                InternalError::Backend("boom".to_string()),
            ];
            for variant in variants {
                assert_eq!(
                    status_of(ApiError::Internal(variant)),
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR
                );
            }
        });
    }
}
