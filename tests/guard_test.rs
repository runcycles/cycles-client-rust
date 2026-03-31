//! Additional tests for ReservationGuard lifecycle behavior.

use runcycles::models::*;
use runcycles::CyclesClient;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup_with_reservation(server: &MockServer) -> CyclesClient {
    let client = CyclesClient::builder("key", server.uri()).build();

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "ALLOW_WITH_CAPS",
            "reservation_id": "rsv_test",
            "affected_scopes": ["tenant:acme", "app:my-app"],
            "expires_at_ms": 1700000060000_u64,
            "caps": {"max_tokens": 500, "max_steps_remaining": 10, "cooldown_ms": 1000}
        })))
        .mount(server)
        .await;

    // Mock extend for heartbeat
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_test/extend"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ACTIVE",
            "expires_at_ms": 1700000120000_u64
        })))
        .mount(server)
        .await;

    // Mock release for drop
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_test/release"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "RELEASED",
            "released": {"unit": "USD_MICROCENTS", "amount": 5000}
        })))
        .mount(server)
        .await;

    client
}

fn make_reserve_request() -> ReservationCreateRequest {
    ReservationCreateRequest::builder()
        .subject(Subject {
            tenant: Some("acme".into()),
            ..Default::default()
        })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .build()
}

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

    let debug = format!("{:?}", guard);
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
