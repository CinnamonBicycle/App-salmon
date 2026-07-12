use std::time::Duration;

use crate::common::{
    auth_value, create_cluster, other_auth_value, spawn_test_server, wait_for_ready_or_failed,
};

#[tokio::test]
async fn unknown_id_is_not_found() {
    let server = spawn_test_server().await;
    let response = server
        .client
        .get(format!(
            "{}/clusters/{}",
            server.base_url,
            ulid::Ulid::nil()
        ))
        .header("authorization", auth_value())
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn owned_by_someone_else_is_not_found() {
    let server = spawn_test_server().await;
    let (_, created) = create_cluster(&server, 30).await;
    let id = created["id"].as_str().expect("id present");

    let response = server
        .client
        .get(format!("{}/clusters/{id}", server.base_url))
        .header("authorization", other_auth_value())
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reports_spawning_then_ready() {
    let server = spawn_test_server().await;
    let (_, created) = create_cluster(&server, 30).await;
    let id = created["id"].as_str().expect("id present");

    let immediate = server
        .client
        .get(format!("{}/clusters/{id}", server.base_url))
        .header("authorization", auth_value())
        .send()
        .await
        .expect("request");
    assert_eq!(immediate.status(), reqwest::StatusCode::OK);
    let immediate_body: serde_json::Value = immediate.json().await.expect("json");
    assert!(
        immediate_body["status"] == "spawning" || immediate_body["status"] == "ready",
        "unexpected status immediately after create: {immediate_body}"
    );

    let info = wait_for_ready_or_failed(&server, id, Duration::from_mins(3)).await;
    assert_eq!(
        info["status"], "ready",
        "cluster did not become ready: {info}"
    );
    assert!(info["scheduled_decommission_at"].is_string());
}
