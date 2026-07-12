use std::time::Duration;

use crate::common::{
    auth_value, create_cluster, spawn_test_server, unregistered_auth_value,
    wait_for_ready_or_failed,
};

#[tokio::test]
async fn valid_request_eventually_becomes_ready() {
    let server = spawn_test_server().await;
    let (status, body) = create_cluster(&server, 30).await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED);
    assert_eq!(body["status"], "spawning");
    let id = body["id"].as_str().expect("id present");

    let info = wait_for_ready_or_failed(&server, id, Duration::from_mins(3)).await;
    assert_eq!(
        info["status"], "ready",
        "cluster did not become ready: {info}"
    );
    assert!(
        info["connection"]["password"]
            .as_str()
            .is_some_and(|p| !p.is_empty())
    );
    assert!(info["connection"]["port"].as_u64().is_some_and(|p| p > 0));
}

#[tokio::test]
async fn pgvector_flag_enables_the_extension() {
    let server = spawn_test_server().await;
    let response = server
        .client
        .post(format!("{}/clusters", server.base_url))
        .header("authorization", auth_value())
        .json(&serde_json::json!({"service": "postgres", "pgvector": true, "ttl_secs": 30}))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let body: serde_json::Value = response.json().await.expect("json");
    let id = body["id"].as_str().expect("id present");

    let info = wait_for_ready_or_failed(&server, id, Duration::from_mins(3)).await;
    assert_eq!(
        info["status"], "ready",
        "cluster did not become ready: {info}"
    );

    let host = info["connection"]["host"].as_str().expect("host");
    let port = info["connection"]["port"].as_u64().expect("port");
    let user = info["connection"]["user"].as_str().expect("user");
    let password = info["connection"]["password"].as_str().expect("password");
    let dbname = info["connection"]["dbname"].as_str().expect("dbname");

    let config = format!("host={host} port={port} user={user} password={password} dbname={dbname}");
    let (client, connection) = tokio_postgres::connect(&config, tokio_postgres::NoTls)
        .await
        .expect("connect to provisioned postgres");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let row = client
        .query_one(
            "SELECT count(*) FROM pg_extension WHERE extname = 'vector'",
            &[],
        )
        .await
        .expect("query pg_extension");
    let count: i64 = row.get(0);
    assert_eq!(count, 1, "pgvector extension should be installed");
}

#[tokio::test]
async fn ttl_below_minimum_is_bad_request() {
    let server = spawn_test_server().await;
    let (status, _body) = create_cluster(&server, 5).await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ttl_above_maximum_is_bad_request() {
    let server = spawn_test_server().await;
    let (status, _body) = create_cluster(&server, 10_000).await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn beyond_quota_is_too_many_requests() {
    let server = spawn_test_server().await;
    let (first, _) = create_cluster(&server, 30).await;
    let (second, _) = create_cluster(&server, 30).await;
    let (third, _) = create_cluster(&server, 30).await;
    assert_eq!(first, reqwest::StatusCode::ACCEPTED);
    assert_eq!(second, reqwest::StatusCode::ACCEPTED);
    assert_eq!(third, reqwest::StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn missing_credentials_is_unauthorized() {
    let server = spawn_test_server().await;
    let response = server
        .client
        .post(format!("{}/clusters", server.base_url))
        .json(&serde_json::json!({"service": "postgres", "ttl_secs": 30}))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unregistered_client_is_unauthorized() {
    let server = spawn_test_server().await;
    let response = server
        .client
        .post(format!("{}/clusters", server.base_url))
        .header("authorization", unregistered_auth_value())
        .json(&serde_json::json!({"service": "postgres", "ttl_secs": 30}))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
}
