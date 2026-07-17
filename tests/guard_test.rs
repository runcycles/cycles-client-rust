//! Additional tests for ReservationGuard lifecycle behavior.

mod common;

use common::{make_reserve_request, setup_with_reservation};
use runcycles::models::*;
use wiremock::MockServer;

#[tokio::test]
async fn guard_accessors_with_caps() {
    let server = MockServer::start().await;
    let client = setup_with_reservation(&server).await;

    let guard = client.reserve(make_reserve_request()).await.unwrap();

    assert_eq!(guard.reservation_id().as_str(), "rsv_test");
    assert_eq!(guard.decision(), Decision::AllowWithCaps);
    assert!(guard.is_capped());
    assert_eq!(guard.expires_at_ms(), Some(1700000060000));
    assert_eq!(guard.affected_scopes().len(), 2);

    let caps = guard.caps().unwrap();
    assert_eq!(caps.max_tokens, Some(500));
    assert_eq!(caps.max_steps_remaining, Some(10));
    assert_eq!(caps.cooldown_ms, Some(1000));

    // Release to clean up
    guard.release("test_done").await.unwrap();
}

#[tokio::test]
async fn guard_extend_manual() {
    let server = MockServer::start().await;
    let client = setup_with_reservation(&server).await;

    let guard = client.reserve(make_reserve_request()).await.unwrap();

    // Manual extend
    guard.extend(60_000).await.unwrap();

    guard.release("done").await.unwrap();
}

#[tokio::test]
async fn guard_debug_format() {
    let server = MockServer::start().await;
    let client = setup_with_reservation(&server).await;

    let guard = client.reserve(make_reserve_request()).await.unwrap();

    let debug = format!("{guard:?}");
    assert!(debug.contains("ReservationGuard"));
    assert!(debug.contains("rsv_test"));
    assert!(debug.contains("AllowWithCaps"));

    guard.release("done").await.unwrap();
}

#[tokio::test]
async fn guard_drop_attempts_release() {
    let server = MockServer::start().await;
    let client = setup_with_reservation(&server).await;

    let guard = client.reserve(make_reserve_request()).await.unwrap();

    // Drop without commit or release — should trigger best-effort release
    drop(guard);

    // Give the spawned release task time to execute
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify release was called (server received the request)
    let received = server.received_requests().await.unwrap();
    let release_calls: Vec<_> = received
        .iter()
        .filter(|r| r.url.path().contains("/release"))
        .collect();
    assert!(
        !release_calls.is_empty(),
        "expected at least one release call from guard drop"
    );
}
