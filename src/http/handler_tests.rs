//! Walking-skeleton milestone: every endpoint, driven end-to-end through the real `axum::Router`
//! via `tower::ServiceExt::oneshot`, against fake-backed dependencies (no real Docker/sudo). This
//! is what proves the HTTP layer's status-code/body mapping is correct independent of whether
//! any real adapter works.

#![cfg(test)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{TimeDelta, Utc};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::auth::ClientRegistry;
use crate::auth::hashing::SecretHash;
use crate::backends::ClusterBackend;
use crate::client_workers::ClientWorkers;
use crate::domain::cluster::ClusterState;
use crate::domain::ids::{ClientId, ClusterId, WorkerUser};
use crate::domain::service_kind::{
    ConnectionInfo, PostgresConnectionInfo, ServiceKind, SupabaseConnectionInfo,
};
use crate::http::{AppState, router};
use crate::ports::clock::{Clock, FakeClock};
use crate::ports::repository::ClusterRepository;
use crate::redacted::Redacted;
use crate::service::cluster_service::{ClusterService, Limits};
use crate::service::deps::{TaskDeps, TaskRegistry};
use crate::test_support::{
    FakeSecretGenerator, FastSucceedingClusterBackend, HangingClusterBackend,
    InMemoryClusterRepository, NoopPrivilegedExecutor,
};

const CLIENT_NAME: &str = "test-agent";
const CLIENT_SECRET: &str = "test-secret";
const OTHER_CLIENT_NAME: &str = "other-agent";
const OTHER_CLIENT_SECRET: &str = "other-secret";

fn test_app() -> (Router, Arc<InMemoryClusterRepository>, Arc<FakeClock>) {
    test_app_with_backend(Arc::new(HangingClusterBackend))
}

fn test_app_with_backend(
    backend: Arc<dyn ClusterBackend>,
) -> (Router, Arc<InMemoryClusterRepository>, Arc<FakeClock>) {
    let repository = Arc::new(InMemoryClusterRepository::new());
    let clock = Arc::new(FakeClock::new(Utc::now()));
    let cluster_service = Arc::new(ClusterService::new(
        repository.clone(),
        clock.clone(),
        Arc::new(FakeSecretGenerator::default()),
        Limits {
            min_ttl: TimeDelta::seconds(30),
            max_ttl: TimeDelta::seconds(3600),
            max_clusters_per_user: 2,
        },
    ));

    let mut clients = HashMap::new();
    clients.insert(ClientId::new(CLIENT_NAME), SecretHash::of(CLIENT_SECRET));
    clients.insert(
        ClientId::new(OTHER_CLIENT_NAME),
        SecretHash::of(OTHER_CLIENT_SECRET),
    );
    let client_registry = Arc::new(ClientRegistry::new(clients));

    // Every test client gets its own account (mirroring one `[[clients]]` entry each in real
    // config) — no test in this file exercises "owner has no configured account" (that's covered
    // directly in `service::spawn_task`'s own unit tests).
    let mut client_workers = HashMap::new();
    client_workers.insert(
        ClientId::new(CLIENT_NAME),
        WorkerUser::new(CLIENT_NAME, 2000, 2000),
    );
    client_workers.insert(
        ClientId::new(OTHER_CLIENT_NAME),
        WorkerUser::new(OTHER_CLIENT_NAME, 2001, 2001),
    );

    let task_deps = Arc::new(TaskDeps {
        repository: repository.clone(),
        client_workers: Arc::new(ClientWorkers::new(client_workers)),
        privileged_exec: Arc::new(NoopPrivilegedExecutor),
        backends: HashMap::from([(ServiceKind::Postgres, backend)]),
        clock: clock.clone(),
        worker_data_dir_base: std::path::PathBuf::from("/var/lib/app_salmon/workers"),
    });

    let state = AppState {
        cluster_service,
        client_registry,
        task_deps,
        task_registry: Arc::new(TaskRegistry::new()),
        spawn_estimate: TimeDelta::seconds(20),
        max_tar_bytes: 1_048_576,
    };

    (router(state), repository, clock)
}

fn auth_header() -> (&'static str, String) {
    (
        "authorization",
        format!("Bearer {CLIENT_NAME}:{CLIENT_SECRET}"),
    )
}

fn other_auth_header() -> (&'static str, String) {
    (
        "authorization",
        format!("Bearer {OTHER_CLIENT_NAME}:{OTHER_CLIENT_SECRET}"),
    )
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("valid json")
}

async fn create_cluster(app: &Router, ttl_secs: i64) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/clusters")
        .header(auth_header().0, auth_header().1)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(
                &json!({"service": "postgres", "pgvector": false, "ttl_secs": ttl_secs}),
            )
            .expect("serialize"),
        ))
        .expect("build request");
    let response = app.clone().oneshot(request).await.expect("call succeeds");
    let status = response.status();
    (status, json_body(response).await)
}

const MULTIPART_BOUNDARY: &str = "app-salmon-test-boundary";

/// Builds a raw `multipart/form-data` body with a `metadata` part (`metadata_json`, sent as-is —
/// callers control whether it's valid JSON) and, unless `omit_project_tar` is set, a
/// `project_tar` part carrying `project_tar` as raw bytes.
fn multipart_body(metadata_json: &str, project_tar: Option<&[u8]>) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"metadata\"\r\n\r\n");
    body.extend_from_slice(metadata_json.as_bytes());
    body.extend_from_slice(b"\r\n");
    if let Some(project_tar) = project_tar {
        body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"project_tar\"; filename=\"project.tar\"\r\n\
              Content-Type: application/x-tar\r\n\r\n",
        );
        body.extend_from_slice(project_tar);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    body
}

async fn create_cluster_multipart(
    app: &Router,
    metadata_json: &str,
    project_tar: Option<&[u8]>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/clusters")
        .header(auth_header().0, auth_header().1)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}"),
        )
        .body(Body::from(multipart_body(metadata_json, project_tar)))
        .expect("build request");
    let response = app.clone().oneshot(request).await.expect("call succeeds");
    let status = response.status();
    (status, json_body(response).await)
}

#[tokio::test]
async fn create_cluster_valid_request_is_accepted_as_spawning() {
    let (app, _repository, _clock) = test_app();
    let (status, body) = create_cluster(&app, 300).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "spawning");
    assert!(body["id"].is_string());
}

#[tokio::test]
async fn create_cluster_without_credentials_is_unauthorized() {
    let (app, _repository, _clock) = test_app();
    let request = Request::builder()
        .method("POST")
        .uri("/clusters")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"service": "postgres", "ttl_secs": 300}))
                .expect("serialize"),
        ))
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_cluster_with_wrong_secret_is_unauthorized() {
    let (app, _repository, _clock) = test_app();
    let request = Request::builder()
        .method("POST")
        .uri("/clusters")
        .header(
            "authorization",
            format!("Bearer {CLIENT_NAME}:wrong-secret"),
        )
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"service": "postgres", "ttl_secs": 300}))
                .expect("serialize"),
        ))
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_cluster_ttl_below_minimum_is_bad_request() {
    let (app, _repository, _clock) = test_app();
    let (status, _body) = create_cluster(&app, 5).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_cluster_ttl_above_maximum_is_bad_request() {
    let (app, _repository, _clock) = test_app();
    let (status, _body) = create_cluster(&app, 10_000).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_cluster_beyond_quota_is_too_many_requests() {
    let (app, _repository, _clock) = test_app();
    let (first_status, _) = create_cluster(&app, 300).await;
    let (second_status, _) = create_cluster(&app, 300).await;
    let (third_status, _) = create_cluster(&app, 300).await;
    assert_eq!(first_status, StatusCode::ACCEPTED);
    assert_eq!(second_status, StatusCode::ACCEPTED);
    assert_eq!(third_status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn create_cluster_supabase_multipart_valid_request_is_accepted_as_spawning() {
    let (app, _repository, _clock) = test_app();
    let metadata = json!({"service": "supabase", "ttl_secs": 300}).to_string();
    let (status, body) =
        create_cluster_multipart(&app, &metadata, Some(b"not a real tar, doesn't matter yet"))
            .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "spawning");
    assert!(body["id"].is_string());
}

#[tokio::test]
async fn create_cluster_supabase_as_json_is_bad_request() {
    let (app, _repository, _clock) = test_app();
    let request = Request::builder()
        .method("POST")
        .uri("/clusters")
        .header(auth_header().0, auth_header().1)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"service": "supabase", "ttl_secs": 300}))
                .expect("serialize"),
        ))
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_cluster_postgres_as_multipart_is_bad_request() {
    let (app, _repository, _clock) = test_app();
    let metadata = json!({"service": "postgres", "ttl_secs": 300}).to_string();
    let (status, _body) =
        create_cluster_multipart(&app, &metadata, Some(b"irrelevant bytes")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_cluster_multipart_missing_project_tar_is_bad_request() {
    let (app, _repository, _clock) = test_app();
    let metadata = json!({"service": "supabase", "ttl_secs": 300}).to_string();
    let (status, _body) = create_cluster_multipart(&app, &metadata, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_cluster_multipart_missing_metadata_is_bad_request() {
    let (app, _repository, _clock) = test_app();
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"project_tar\"; filename=\"project.tar\"\r\n\
          Content-Type: application/x-tar\r\n\r\nirrelevant bytes\r\n",
    );
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/clusters")
        .header(auth_header().0, auth_header().1)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}"),
        )
        .body(Body::from(body))
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_cluster_multipart_malformed_metadata_json_is_bad_request() {
    let (app, _repository, _clock) = test_app();
    let (status, _body) =
        create_cluster_multipart(&app, "not valid json", Some(b"irrelevant bytes")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_cluster_multipart_ignores_unrecognized_parts() {
    let (app, _repository, _clock) = test_app();
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"something_else\"\r\n\r\nignored\r\n",
    );
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"metadata\"\r\n\r\n");
    body.extend_from_slice(
        json!({"service": "supabase", "ttl_secs": 300})
            .to_string()
            .as_bytes(),
    );
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"project_tar\"; filename=\"project.tar\"\r\n\
          Content-Type: application/x-tar\r\n\r\nirrelevant bytes\r\n",
    );
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/clusters")
        .header(auth_header().0, auth_header().1)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}"),
        )
        .body(Body::from(body))
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn create_cluster_multipart_project_tar_over_the_body_limit_is_bad_request() {
    let (app, _repository, _clock) = test_app();
    let metadata = json!({"service": "supabase", "ttl_secs": 300}).to_string();
    let oversized = vec![0_u8; 2 * 1_048_576];
    let (status, _body) = create_cluster_multipart(&app, &metadata, Some(&oversized)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_cluster_unknown_id_is_not_found() {
    let (app, _repository, _clock) = test_app();
    let request = Request::builder()
        .method("GET")
        .uri(format!("/clusters/{}", ClusterId::new(ulid::Ulid::nil())))
        .header(auth_header().0, auth_header().1)
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_cluster_owned_by_someone_else_is_not_found() {
    let (app, _repository, _clock) = test_app();
    let (_, created) = create_cluster(&app, 300).await;
    let id = created["id"].as_str().expect("id present");

    let request = Request::builder()
        .method("GET")
        .uri(format!("/clusters/{id}"))
        .header(other_auth_header().0, other_auth_header().1)
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_cluster_while_spawning_returns_200() {
    let (app, _repository, _clock) = test_app();
    let (_, created) = create_cluster(&app, 300).await;
    let id = created["id"].as_str().expect("id present");

    let request = Request::builder()
        .method("GET")
        .uri(format!("/clusters/{id}"))
        .header(auth_header().0, auth_header().1)
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["status"], "spawning");
}

#[tokio::test]
async fn get_cluster_ready_returns_200_with_connection_info() {
    let (app, repository, clock) = test_app();
    let (_, created) = create_cluster(&app, 300).await;
    let id_str = created["id"].as_str().expect("id present").to_string();
    let id: ClusterId = id_str.parse().expect("valid ulid");

    let ready_at = clock.now();
    repository
        .update_state(
            &id,
            &ClusterState::Ready {
                ready_at,
                decommission_at: ready_at + TimeDelta::seconds(300),
                connection: ConnectionInfo::Postgres(PostgresConnectionInfo {
                    host: "127.0.0.1".to_string(),
                    port: 55432,
                    dbname: "app_salmon".to_string(),
                    user: "app_salmon".to_string(),
                    password: Redacted::new("hunter2".to_string()),
                }),
            },
        )
        .await
        .expect("mark ready");

    let request = Request::builder()
        .method("GET")
        .uri(format!("/clusters/{id_str}"))
        .header(auth_header().0, auth_header().1)
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["status"], "ready");
    assert_eq!(body["connection"]["password"], "hunter2");
    assert_eq!(body["connection"]["port"], 55432);
    assert_eq!(body["connection"]["kind"], "postgres");
}

#[tokio::test]
async fn get_cluster_ready_returns_200_with_supabase_connection_info() {
    let (app, repository, clock) = test_app();
    let (_, created) = create_cluster(&app, 300).await;
    let id_str = created["id"].as_str().expect("id present").to_string();
    let id: ClusterId = id_str.parse().expect("valid ulid");

    let ready_at = clock.now();
    repository
        .update_state(
            &id,
            &ClusterState::Ready {
                ready_at,
                decommission_at: ready_at + TimeDelta::seconds(300),
                connection: ConnectionInfo::Supabase(SupabaseConnectionInfo {
                    api_url: "http://127.0.0.1:8000".to_string(),
                    postgres: PostgresConnectionInfo {
                        host: "127.0.0.1".to_string(),
                        port: 55432,
                        dbname: "postgres".to_string(),
                        user: "postgres".to_string(),
                        password: Redacted::new("hunter2".to_string()),
                    },
                    anon_key: Redacted::new("anon.jwt".to_string()),
                    service_role_key: Redacted::new("service.jwt".to_string()),
                    jwt_secret: Redacted::new("jwt-secret-value".to_string()),
                }),
            },
        )
        .await
        .expect("mark ready");

    let request = Request::builder()
        .method("GET")
        .uri(format!("/clusters/{id_str}"))
        .header(auth_header().0, auth_header().1)
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["status"], "ready");
    assert_eq!(body["connection"]["kind"], "supabase");
    assert_eq!(body["connection"]["api_url"], "http://127.0.0.1:8000");
    assert_eq!(body["connection"]["postgres"]["port"], 55432);
    assert_eq!(body["connection"]["anon_key"], "anon.jwt");
    assert_eq!(body["connection"]["service_role_key"], "service.jwt");
    assert_eq!(body["connection"]["jwt_secret"], "jwt-secret-value");
}

#[tokio::test]
async fn get_cluster_failed_returns_200_with_sanitized_error() {
    let (app, repository, clock) = test_app();
    let (_, created) = create_cluster(&app, 300).await;
    let id_str = created["id"].as_str().expect("id present").to_string();
    let id: ClusterId = id_str.parse().expect("valid ulid");

    repository
        .update_state(
            &id,
            &ClusterState::Failed {
                failed_at: clock.now(),
                error_summary: "container did not become healthy in time".to_string(),
            },
        )
        .await
        .expect("mark failed");

    let request = Request::builder()
        .method("GET")
        .uri(format!("/clusters/{id_str}"))
        .header(auth_header().0, auth_header().1)
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["status"], "failed");
    assert_eq!(body["error"], "container did not become healthy in time");
}

#[tokio::test]
async fn get_cluster_deleting_returns_410() {
    let (app, repository, clock) = test_app();
    let (_, created) = create_cluster(&app, 300).await;
    let id_str = created["id"].as_str().expect("id present").to_string();
    let id: ClusterId = id_str.parse().expect("valid ulid");

    repository
        .update_state(
            &id,
            &ClusterState::Deleting {
                deleting_since: clock.now(),
                reason: crate::domain::cluster::DeleteReason::UserRequested,
            },
        )
        .await
        .expect("mark deleting");

    let request = Request::builder()
        .method("GET")
        .uri(format!("/clusters/{id_str}"))
        .header(auth_header().0, auth_header().1)
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::GONE);
}

#[tokio::test]
async fn list_clusters_returns_only_the_callers_clusters() {
    let (app, _repository, _clock) = test_app();
    create_cluster(&app, 300).await;

    let other_create = Request::builder()
        .method("POST")
        .uri("/clusters")
        .header(other_auth_header().0, other_auth_header().1)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"service": "postgres", "ttl_secs": 300}))
                .expect("serialize"),
        ))
        .expect("build request");
    app.clone()
        .oneshot(other_create)
        .await
        .expect("call succeeds");

    let request = Request::builder()
        .method("GET")
        .uri("/clusters")
        .header(auth_header().0, auth_header().1)
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body.as_array().expect("array").len(), 1);
}

#[tokio::test]
async fn delete_unknown_cluster_is_not_found() {
    let (app, _repository, _clock) = test_app();
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/clusters/{}", ClusterId::new(ulid::Ulid::nil())))
        .header(auth_header().0, auth_header().1)
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_cluster_is_accepted_and_idempotent() {
    let (app, _repository, _clock) = test_app();
    let (_, created) = create_cluster(&app, 300).await;
    let id_str = created["id"].as_str().expect("id present").to_string();

    let delete_once = Request::builder()
        .method("DELETE")
        .uri(format!("/clusters/{id_str}"))
        .header(auth_header().0, auth_header().1)
        .body(Body::empty())
        .expect("build request");
    let response = app
        .clone()
        .oneshot(delete_once)
        .await
        .expect("call succeeds");
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let delete_again = Request::builder()
        .method("DELETE")
        .uri(format!("/clusters/{id_str}"))
        .header(auth_header().0, auth_header().1)
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(delete_again).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn delete_someone_elses_cluster_is_not_found() {
    let (app, _repository, _clock) = test_app();
    let (_, created) = create_cluster(&app, 300).await;
    let id_str = created["id"].as_str().expect("id present").to_string();

    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/clusters/{id_str}"))
        .header(other_auth_header().0, other_auth_header().1)
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_cluster_background_spawn_task_runs_to_completion() {
    // Unlike the other tests (which use `HangingClusterBackend` so directly-set repository state
    // isn't racing the background task), this uses a backend that actually finishes — exercising
    // `launch_spawn`'s wrapper all the way through, including the task-registry unregister call
    // that only happens once `spawn_task::run` returns.
    let (app, _repository, _clock) = test_app_with_backend(Arc::new(FastSucceedingClusterBackend));
    let (_, created) = create_cluster(&app, 300).await;
    let id = created["id"].as_str().expect("id present").to_string();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/clusters/{id}"))
            .header(auth_header().0, auth_header().1)
            .body(Body::empty())
            .expect("build request");
        let response = app.clone().oneshot(request).await.expect("call succeeds");
        let body = json_body(response).await;
        if body["status"] == "ready" {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "cluster never became ready: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn delete_ready_cluster_starts_a_teardown_task() {
    // Deleting a `Spawning` cluster (the other delete tests) exercises `DeleteOutcome::CancelSpawn`.
    // This exercises the other branch, `DeleteOutcome::StartTeardown`, which only fires for a
    // cluster that isn't `Spawning` — so this uses the fast-succeeding backend to actually reach
    // `Ready` first.
    let (app, _repository, _clock) = test_app_with_backend(Arc::new(FastSucceedingClusterBackend));
    let (_, created) = create_cluster(&app, 300).await;
    let id = created["id"].as_str().expect("id present").to_string();

    let ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/clusters/{id}"))
            .header(auth_header().0, auth_header().1)
            .body(Body::empty())
            .expect("build request");
        let response = app.clone().oneshot(request).await.expect("call succeeds");
        if json_body(response).await["status"] == "ready" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < ready_deadline,
            "cluster never became ready"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let delete_request = Request::builder()
        .method("DELETE")
        .uri(format!("/clusters/{id}"))
        .header(auth_header().0, auth_header().1)
        .body(Body::empty())
        .expect("build request");
    let delete_response = app
        .clone()
        .oneshot(delete_request)
        .await
        .expect("call succeeds");
    assert_eq!(delete_response.status(), StatusCode::ACCEPTED);

    let gone_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/clusters/{id}"))
            .header(auth_header().0, auth_header().1)
            .body(Body::empty())
            .expect("build request");
        let response = app.clone().oneshot(request).await.expect("call succeeds");
        if response.status() == StatusCode::NOT_FOUND {
            return;
        }
        assert!(
            tokio::time::Instant::now() < gone_deadline,
            "cluster was never fully torn down"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn openapi_json_is_served_and_documents_the_cluster_routes() {
    let (app, _repository, _clock) = test_app();
    let request = Request::builder()
        .method("GET")
        .uri("/openapi.json")
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    assert_eq!(response.status(), StatusCode::OK);

    let spec = json_body(response).await;
    assert!(
        spec.get("openapi").is_some(),
        "missing openapi version field: {spec}"
    );
    let paths = spec
        .get("paths")
        .and_then(Value::as_object)
        .expect("paths object");
    assert!(paths.contains_key("/clusters"));
    assert!(paths.contains_key("/clusters/{id}"));
}

#[tokio::test]
async fn root_serves_swagger_ui_rather_than_404ing() {
    let (app, _repository, _clock) = test_app();
    let request = Request::builder()
        .method("GET")
        .uri("/")
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("call succeeds");
    let status = response.status();
    assert!(
        status.is_success() || status.is_redirection(),
        "expected swagger UI to be reachable at \"/\", got {status}"
    );
    assert_ne!(status, StatusCode::NOT_FOUND);
}
