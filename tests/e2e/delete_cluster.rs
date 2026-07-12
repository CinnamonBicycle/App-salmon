use std::time::Duration;

use crate::common::{
    auth_value, create_cluster, other_auth_value, spawn_test_server, wait_for_not_found,
    wait_for_ready_or_failed,
};

#[tokio::test]
async fn unknown_id_is_not_found() {
    let server = spawn_test_server().await;
    let response = server
        .client
        .delete(format!(
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
async fn someone_elses_cluster_is_not_found() {
    let server = spawn_test_server().await;
    let (_, created) = create_cluster(&server, 30).await;
    let id = created["id"].as_str().expect("id present");

    let response = server
        .client
        .delete(format!("{}/clusters/{id}", server.base_url))
        .header("authorization", other_auth_value())
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ready_cluster_is_deleted_and_eventually_gone() {
    let server = spawn_test_server().await;
    let (_, created) = create_cluster(&server, 30).await;
    let id = created["id"].as_str().expect("id present");
    let info = wait_for_ready_or_failed(&server, id, Duration::from_mins(3)).await;
    assert_eq!(
        info["status"], "ready",
        "cluster did not become ready: {info}"
    );

    let delete_response = server
        .client
        .delete(format!("{}/clusters/{id}", server.base_url))
        .header("authorization", auth_value())
        .send()
        .await
        .expect("request");
    assert_eq!(delete_response.status(), reqwest::StatusCode::ACCEPTED);

    wait_for_not_found(&server, id, Duration::from_mins(1)).await;
}

#[tokio::test]
async fn is_idempotent() {
    let server = spawn_test_server().await;
    let (_, created) = create_cluster(&server, 30).await;
    let id = created["id"].as_str().expect("id present");

    let first = server
        .client
        .delete(format!("{}/clusters/{id}", server.base_url))
        .header("authorization", auth_value())
        .send()
        .await
        .expect("request");
    assert_eq!(first.status(), reqwest::StatusCode::ACCEPTED);

    let second = server
        .client
        .delete(format!("{}/clusters/{id}", server.base_url))
        .header("authorization", auth_value())
        .send()
        .await
        .expect("request");
    assert_eq!(second.status(), reqwest::StatusCode::ACCEPTED);

    wait_for_not_found(&server, id, Duration::from_mins(1)).await;
}

#[tokio::test]
async fn deleting_while_spawning_cancels_and_cleans_up() {
    let server = spawn_test_server().await;
    let (_, created) = create_cluster(&server, 30).await;
    let id = created["id"].as_str().expect("id present");

    // Delete immediately, before the container has necessarily finished starting.
    let delete_response = server
        .client
        .delete(format!("{}/clusters/{id}", server.base_url))
        .header("authorization", auth_value())
        .send()
        .await
        .expect("request");
    assert_eq!(delete_response.status(), reqwest::StatusCode::ACCEPTED);

    wait_for_not_found(&server, id, Duration::from_mins(1)).await;
}
