//! Tests for the alternate-beat heartbeat extension cadence.
//!
//! The protocol's `extend_by_ms` is relative to the reservation's **current
//! `expires_at_ms`**, not to request time, so extending on every `ttl/2` beat
//! drifts the expiry outward by `ttl/2` per beat. The heartbeat must instead
//! extend on the first beat, skip exactly one beat after each successful
//! extend, and retry on the very next beat after a failure. These tests run a
//! real short-TTL heartbeat against wiremock and count extend calls per beat.

use std::time::Duration;

use runcycles::models::*;
use runcycles::CyclesClient;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// TTL chosen so the heartbeat interval is the 1-second floor:
/// `(1000 / 2).max(1000) = 1000 ms` per beat.
const TTL_MS: u64 = 1_000;
const BEAT: Duration = Duration::from_millis(1_000);
/// Margin past the last expected beat: long enough to absorb scheduler and
/// loopback-HTTP latency, short of the following beat.
const MARGIN: Duration = Duration::from_millis(450);

fn extend_path(rsv_id: &str) -> String {
    format!("/v1/reservations/{rsv_id}/extend")
}

async fn reserve_short_ttl(server: &MockServer, rsv_id: &str) -> runcycles::ReservationGuard {
    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "ALLOW",
            "reservation_id": rsv_id,
            "affected_scopes": ["tenant:acme"],
            "expires_at_ms": 1700000001000_u64
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/reservations/{rsv_id}/release")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "RELEASED",
            "released": {"unit": "USD_MICROCENTS", "amount": 5000}
        })))
        .mount(server)
        .await;

    let client = CyclesClient::builder("key", server.uri()).build();
    let req = ReservationCreateRequest::builder()
        .subject(Subject {
            tenant: Some("acme".into()),
            ..Default::default()
        })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .ttl_ms(TTL_MS)
        .build();
    client.reserve(req).await.unwrap()
}

/// Count extend calls received so far and assert each carried
/// `extend_by_ms == TTL_MS`.
async fn extend_calls(server: &MockServer, rsv_id: &str) -> usize {
    let requests = server.received_requests().await.unwrap();
    let extends: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == extend_path(rsv_id))
        .collect();
    for r in &extends {
        let body: serde_json::Value = r.body_json().unwrap();
        assert_eq!(
            body["extend_by_ms"], TTL_MS,
            "extend amount must stay ttl_ms"
        );
    }
    extends.len()
}

/// Beat 1 extends, beat 2 is skipped (previous extend succeeded), beat 3
/// extends again: exactly 2 extend calls across 3 beats — not 3, which is
/// the every-beat drift behavior this suite pins against.
#[tokio::test]
async fn heartbeat_extends_on_alternate_beats() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_alt";

    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ACTIVE",
            "expires_at_ms": 1700000002000_u64
        })))
        .mount(&server)
        .await;

    let guard = reserve_short_ttl(&server, rsv_id).await;

    // Past beat 1: the FIRST beat must extend (only ttl/2 lifetime remains).
    tokio::time::sleep(BEAT + MARGIN).await;
    assert_eq!(extend_calls(&server, rsv_id).await, 1, "beat 1 must extend");

    // Past beat 2: previous extend succeeded, so this beat is skipped.
    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id).await,
        1,
        "beat 2 must be skipped after a successful extend"
    );

    // Past beat 3: the skip lasts exactly one beat.
    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id).await,
        2,
        "beat 3 must extend again"
    );

    guard.release("test done").await.unwrap();
}

/// A failed extend (HTTP 500) must be retried on the very next beat, and the
/// skip applies only after the retry succeeds.
#[tokio::test]
async fn heartbeat_retries_next_beat_after_failed_extend() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_fail";

    // First extend attempt fails...
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "INTERNAL",
            "message": "boom"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // ...subsequent attempts succeed.
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ACTIVE",
            "expires_at_ms": 1700000002000_u64
        })))
        .mount(&server)
        .await;

    let guard = reserve_short_ttl(&server, rsv_id).await;

    // Past beat 1: extend attempted, fails with 500.
    tokio::time::sleep(BEAT + MARGIN).await;
    assert_eq!(extend_calls(&server, rsv_id).await, 1, "beat 1 attempts");

    // Past beat 2: failure must NOT skip — retry immediately, succeeds.
    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id).await,
        2,
        "beat 2 must retry after the failed extend"
    );

    // Past beat 3: the successful retry earns the one-beat skip.
    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id).await,
        2,
        "beat 3 must be skipped after the successful retry"
    );

    guard.release("test done").await.unwrap();
}

/// An HTTP-success extend whose status is not ACTIVE (forward-compat
/// `Unknown`) is not proof the TTL was extended — it must be treated like a
/// failure and retried on the next beat.
#[tokio::test]
async fn heartbeat_treats_non_active_status_as_failure() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_unknown";

    // First extend returns HTTP 200 with an unrecognized status...
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "SUSPENDED",
            "expires_at_ms": 1700000002000_u64
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // ...subsequent attempts return ACTIVE.
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ACTIVE",
            "expires_at_ms": 1700000002000_u64
        })))
        .mount(&server)
        .await;

    let guard = reserve_short_ttl(&server, rsv_id).await;

    // Past beat 2: beat 1's non-ACTIVE response must not earn a skip, so
    // beat 2 retries — 2 calls total.
    tokio::time::sleep(BEAT + BEAT + MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id).await,
        2,
        "non-ACTIVE status must be retried on the next beat"
    );

    // Past beat 3: the ACTIVE retry earns the one-beat skip.
    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id).await,
        2,
        "beat 3 must be skipped after the ACTIVE retry"
    );

    guard.release("test done").await.unwrap();
}
