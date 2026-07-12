use std::time::Duration;

use crate::common::{
    create_cluster, spawn_test_server, wait_for_not_found, wait_for_ready_or_failed,
};

/// Uses the 30s TTL floor. `decommission_at` is anchored to `ready_at` (not creation time), so
/// this is a real 30s wait *after* the cluster becomes ready, regardless of how long the
/// container itself took to start — plus margin for the reaper's poll interval and teardown.
#[tokio::test]
async fn cluster_is_automatically_deleted_after_its_ttl() {
    let server = spawn_test_server().await;
    let (_, created) = create_cluster(&server, 30).await;
    let id = created["id"].as_str().expect("id present");

    let info = wait_for_ready_or_failed(&server, id, Duration::from_mins(3)).await;
    assert_eq!(
        info["status"], "ready",
        "cluster did not become ready: {info}"
    );

    wait_for_not_found(
        &server,
        id,
        Duration::from_mins(1) + Duration::from_secs(30),
    )
    .await;
}
