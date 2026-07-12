use crate::common::{auth_value, create_cluster, other_auth_value, spawn_test_server};

#[tokio::test]
async fn returns_only_the_callers_clusters() {
    let server = spawn_test_server().await;
    create_cluster(&server, 30).await;

    let other_create = server
        .client
        .post(format!("{}/clusters", server.base_url))
        .header("authorization", other_auth_value())
        .json(&serde_json::json!({"service": "postgres", "ttl_secs": 30}))
        .send()
        .await
        .expect("request");
    assert_eq!(other_create.status(), reqwest::StatusCode::ACCEPTED);

    let response = server
        .client
        .get(format!("{}/clusters", server.base_url))
        .header("authorization", auth_value())
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("json");
    let entries = body.as_array().expect("array");
    assert_eq!(
        entries.len(),
        1,
        "expected exactly the caller's own cluster: {body}"
    );
}

#[tokio::test]
async fn empty_for_a_caller_with_no_clusters() {
    let server = spawn_test_server().await;
    let response = server
        .client
        .get(format!("{}/clusters", server.base_url))
        .header("authorization", auth_value())
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("json");
    assert!(body.as_array().expect("array").is_empty());
}
