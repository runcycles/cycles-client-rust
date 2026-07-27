//! End-to-end tests for the expired-commit event fallback and for
//! `Retry-After`-aware commit retry.
//!
//! A commit records spend that already happened, so a `RESERVATION_EXPIRED`
//! rejection (commit landed after the grace period; the server already
//! returned the reserved budget) must not silently drop the spend.
//! `ReservationGuard::commit` recovers it as a post-hoc direct-debit event
//! (`POST /v1/events`) reusing the commit's idempotency key. These tests
//! drive the public API — reserve → commit — and assert on the requests the
//! mock server received.

mod common;

use std::time::Duration;

use common::{make_reserve_request, mount_extend, mount_reserve_allow};
use runcycles::models::*;
use runcycles::{CyclesClient, Error, ReservationGuard};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_client(server: &MockServer) -> CyclesClient {
    CyclesClient::builder("key", server.uri())
        .retry_enabled(true)
        .retry_max_attempts(3)
        .retry_initial_delay(Duration::from_millis(10))
        .retry_multiplier(2.0)
        .retry_max_delay(Duration::from_millis(50))
        .build()
}

async fn reserve(client: &CyclesClient) -> ReservationGuard {
    client
        .reserve(make_reserve_request())
        .await
        .expect("reserve should succeed")
}

/// Mount the commit endpoint rejecting with 409 `RESERVATION_EXPIRED`.
async fn mount_commit_expired(server: &MockServer, reservation_id: &str) {
    Mock::given(method("POST"))
        .and(path(format!("/v1/reservations/{reservation_id}/commit")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "RESERVATION_EXPIRED",
            "message": "Reservation expired",
            "request_id": "req-expired"
        })))
        .expect(1)
        .mount(server)
        .await;
}

/// Requests the server received on the given path suffix.
async fn calls_to(server: &MockServer, suffix: &str) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().ends_with(suffix))
        .collect()
}

#[tokio::test]
async fn expired_commit_recovers_via_event() {
    let server = MockServer::start().await;
    let client = test_client(&server);
    mount_reserve_allow(&server, "rsv_exp").await;
    mount_extend(&server, "rsv_exp").await;
    mount_commit_expired(&server, "rsv_exp").await;

    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "APPLIED",
            "event_id": "evt_rec_1",
            "charged": {"unit": "USD_MICROCENTS", "amount": 4200}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let commit_key = IdempotencyKey::new("commit-key-exp-1");
    let guard = reserve(&client).await;
    let resp = guard
        .commit(
            CommitRequest::builder()
                .idempotency_key(commit_key.clone())
                .actual(Amount::usd_microcents(4200))
                .metadata(json!({"trace": "abc"}))
                .build(),
        )
        .await
        .expect("event fallback should salvage the expired commit");

    // Recovery is visible in the returned result.
    assert_eq!(resp.status, CommitStatus::RecoveredViaEvent);
    assert!(resp.is_recovered_via_event());
    assert_eq!(resp.recovered_via_event, Some(EventId::new("evt_rec_1")));
    assert_eq!(resp.charged, Amount::usd_microcents(4200));

    // The event reused the commit's idempotency key (header and body):
    // exactly-once across the separate event namespace.
    let events = calls_to(&server, "/events").await;
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(
        event
            .headers
            .get("X-Idempotency-Key")
            .expect("event must carry idempotency key"),
        commit_key.as_str()
    );
    let body: serde_json::Value = serde_json::from_slice(&event.body).unwrap();
    assert_eq!(body["idempotency_key"], commit_key.as_str());

    // Subject + action come from the reservation; actual from the commit.
    assert_eq!(body["subject"]["tenant"], "acme");
    assert_eq!(body["action"]["kind"], "llm.completion");
    assert_eq!(body["action"]["name"], "gpt-4o");
    assert_eq!(body["actual"]["unit"], "USD_MICROCENTS");
    assert_eq!(body["actual"]["amount"], 4200);

    // Commit metadata carried over, plus the recovery markers.
    assert_eq!(body["metadata"]["trace"], "abc");
    assert_eq!(body["metadata"]["recovered_reservation_id"], "rsv_exp");
    assert_eq!(
        body["metadata"]["recovery_reason"],
        "commit_after_reservation_expired"
    );

    // No overage policy: the server default ALLOW_IF_AVAILABLE never rejects.
    assert!(
        body.get("overage_policy").is_none(),
        "fallback event must not set an overage_policy"
    );
}

#[tokio::test]
async fn expired_commit_preserves_non_object_metadata() {
    let server = MockServer::start().await;
    let client = test_client(&server);
    mount_reserve_allow(&server, "rsv_meta").await;
    mount_extend(&server, "rsv_meta").await;
    mount_commit_expired(&server, "rsv_meta").await;

    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "APPLIED",
            "event_id": "evt_meta"
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Non-object commit metadata (legal JSON, if unusual) must survive the
    // fallback: it is preserved under "original_metadata" so the recovery
    // markers can be added alongside it.
    let guard = reserve(&client).await;
    guard
        .commit(
            CommitRequest::builder()
                .actual(Amount::usd_microcents(100))
                .metadata(json!("free-form note"))
                .build(),
        )
        .await
        .expect("fallback should succeed");

    let events = calls_to(&server, "/events").await;
    assert_eq!(events.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&events[0].body).unwrap();
    assert_eq!(body["metadata"]["original_metadata"], "free-form note");
    assert_eq!(body["metadata"]["recovered_reservation_id"], "rsv_meta");
    assert_eq!(
        body["metadata"]["recovery_reason"],
        "commit_after_reservation_expired"
    );
}

#[tokio::test]
async fn event_fallback_failure_surfaces_both_errors() {
    let server = MockServer::start().await;
    let client = test_client(&server);
    mount_reserve_allow(&server, "rsv_exp2").await;
    mount_extend(&server, "rsv_exp2").await;
    mount_commit_expired(&server, "rsv_exp2").await;

    // Non-retryable event failure: a single attempt, then give up.
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "INVALID_REQUEST",
            "message": "Bad event",
            "request_id": "req-evt-bad"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let guard = reserve(&client).await;
    let err = guard
        .commit(
            CommitRequest::builder()
                .actual(Amount::usd_microcents(4200))
                .build(),
        )
        .await
        .unwrap_err();

    // The caller sees both: the commit expired AND recovery failed.
    match &err {
        Error::CommitRecoveryFailed {
            reservation_id,
            commit_error,
            event_error,
        } => {
            assert_eq!(reservation_id, "rsv_exp2");
            assert_eq!(
                commit_error.error_code(),
                Some(ErrorCode::ReservationExpired)
            );
            assert_eq!(event_error.error_code(), Some(ErrorCode::InvalidRequest));
        }
        other => panic!("expected CommitRecoveryFailed, got {other:?}"),
    }
    assert!(!err.is_retryable(), "recovery failure is final");
    let msg = err.to_string();
    assert!(msg.contains("rsv_exp2"));
    assert!(msg.contains("NOT recorded"));
    assert!(msg.contains("Reservation expired"));
    assert!(msg.contains("Bad event"));
}

#[tokio::test]
async fn event_fallback_retries_transient_failures_with_same_key() {
    let server = MockServer::start().await;
    let client = test_client(&server);
    mount_reserve_allow(&server, "rsv_exp3").await;
    mount_extend(&server, "rsv_exp3").await;
    mount_commit_expired(&server, "rsv_exp3").await;

    // First event attempt fails with a retryable 500, the retry succeeds.
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "INTERNAL_ERROR",
            "message": "Temporary failure",
            "request_id": "req-evt-500"
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "APPLIED",
            "event_id": "evt_rec_3"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let guard = reserve(&client).await;
    let resp = guard
        .commit(
            CommitRequest::builder()
                .actual(Amount::usd_microcents(5000))
                .build(),
        )
        .await
        .expect("bounded event retry should salvage the fallback");

    assert_eq!(resp.status, CommitStatus::RecoveredViaEvent);
    assert_eq!(resp.recovered_via_event, Some(EventId::new("evt_rec_3")));
    // Server sent no charged amount: fall back to the commit's actual.
    assert_eq!(resp.charged, Amount::usd_microcents(5000));

    // Both event attempts reused the same idempotency key.
    let events = calls_to(&server, "/events").await;
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].headers.get("X-Idempotency-Key"),
        events[1].headers.get("X-Idempotency-Key"),
    );
}

#[tokio::test]
async fn reservation_finalized_is_not_recovered() {
    let server = MockServer::start().await;
    let client = test_client(&server);
    mount_reserve_allow(&server, "rsv_fin").await;
    mount_extend(&server, "rsv_fin").await;

    // FINALIZED means the reservation was already committed or released —
    // the spend is not lost, so no event fallback must fire.
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_fin/commit"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "RESERVATION_FINALIZED",
            "message": "Reservation already finalized",
            "request_id": "req-fin"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let guard = reserve(&client).await;
    let err = guard
        .commit(
            CommitRequest::builder()
                .actual(Amount::usd_microcents(5000))
                .build(),
        )
        .await
        .unwrap_err();

    assert_eq!(err.error_code(), Some(ErrorCode::ReservationFinalized));
    assert!(
        calls_to(&server, "/events").await.is_empty(),
        "RESERVATION_FINALIZED must not trigger the event fallback"
    );
}

#[tokio::test]
async fn commit_429_waits_at_least_retry_after() {
    let server = MockServer::start().await;
    let client = test_client(&server);
    mount_reserve_allow(&server, "rsv_429").await;
    mount_extend(&server, "rsv_429").await;

    // 429 with Retry-After: 1 (seconds), then success. The configured
    // backoff (10ms initial, 50ms cap) is far below the server's delay,
    // so honoring Retry-After is observable in wall-clock time.
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_429/commit"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .set_body_json(json!({
                    "error": "LIMIT_EXCEEDED",
                    "message": "Rate limited",
                    "request_id": "req-429"
                })),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_429/commit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "COMMITTED",
            "charged": {"unit": "USD_MICROCENTS", "amount": 5000}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let guard = reserve(&client).await;
    let started = std::time::Instant::now();
    let resp = guard
        .commit(
            CommitRequest::builder()
                .actual(Amount::usd_microcents(5000))
                .build(),
        )
        .await
        .expect("retry should succeed after the rate-limit window");
    let elapsed = started.elapsed();

    assert_eq!(resp.status, CommitStatus::Committed);
    assert!(
        elapsed >= Duration::from_millis(950),
        "retry must wait at least the server's Retry-After (waited {elapsed:?})"
    );
    assert_eq!(
        calls_to(&server, "/commit").await.len(),
        2,
        "expected initial 429 + one retry"
    );
}

#[tokio::test]
async fn commit_429_without_parseable_body_is_still_retried_honoring_retry_after() {
    let server = MockServer::start().await;
    let client = test_client(&server);
    mount_reserve_allow(&server, "rsv_429_raw").await;
    mount_extend(&server, "rsv_429_raw").await;

    // 429 whose body is NOT the JSON error envelope (proxy/LB interposition):
    // no typed error code can be parsed, but the status alone must classify
    // the response as retryable and the Retry-After header must be honored.
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_429_raw/commit"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .set_body_string("<html>Too Many Requests</html>"),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_429_raw/commit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "COMMITTED",
            "charged": {"unit": "USD_MICROCENTS", "amount": 5000}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let guard = reserve(&client).await;
    let started = std::time::Instant::now();
    let resp = guard
        .commit(
            CommitRequest::builder()
                .actual(Amount::usd_microcents(5000))
                .build(),
        )
        .await
        .expect("bodyless 429 must be retried by status alone");
    let elapsed = started.elapsed();

    assert_eq!(resp.status, CommitStatus::Committed);
    assert!(
        elapsed >= Duration::from_millis(950),
        "retry must honor the Retry-After header even without a parseable \
         body (waited {elapsed:?})"
    );
    assert_eq!(calls_to(&server, "/commit").await.len(), 2);
}

#[tokio::test]
async fn commit_410_with_mangled_body_still_recovers_via_event() {
    let server = MockServer::start().await;
    let client = test_client(&server);
    mount_reserve_allow(&server, "rsv_410").await;
    mount_extend(&server, "rsv_410").await;

    // HTTP 410 (Gone) with a non-JSON body: no RESERVATION_EXPIRED code can
    // be parsed, but the status alone means the reservation is settled —
    // the event fallback must still fire.
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_410/commit"))
        .respond_with(ResponseTemplate::new(410).set_body_string("gone (not json)"))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "APPLIED",
            "event_id": "evt_410"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let guard = reserve(&client).await;
    let resp = guard
        .commit(
            CommitRequest::builder()
                .actual(Amount::usd_microcents(4200))
                .build(),
        )
        .await
        .expect("a mangled 410 body must still trigger event recovery");

    assert_eq!(resp.status, CommitStatus::RecoveredViaEvent);
    assert_eq!(resp.recovered_via_event, Some(EventId::new("evt_410")));
}

#[tokio::test]
async fn event_fallback_rejects_non_applied_status_as_recovery() {
    let server = MockServer::start().await;
    let client = test_client(&server);
    mount_reserve_allow(&server, "rsv_nap").await;
    mount_extend(&server, "rsv_nap").await;
    mount_commit_expired(&server, "rsv_nap").await;

    // HTTP 200 whose status is not APPLIED (an unknown future value): NOT
    // proof the spend was recorded — must be treated as recovery failure.
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "PENDING_SETTLEMENT",
            "event_id": "evt_pending"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let guard = reserve(&client).await;
    let err = guard
        .commit(
            CommitRequest::builder()
                .actual(Amount::usd_microcents(4200))
                .build(),
        )
        .await
        .unwrap_err();

    match &err {
        Error::CommitRecoveryFailed {
            reservation_id,
            commit_error,
            event_error,
        } => {
            assert_eq!(reservation_id, "rsv_nap");
            assert_eq!(
                commit_error.error_code(),
                Some(ErrorCode::ReservationExpired)
            );
            let event_msg = event_error.to_string();
            assert!(
                event_msg.contains("unexpected status"),
                "event error must convey the unexpected status: {event_msg}"
            );
            assert!(event_msg.contains("Unknown"));
        }
        other => panic!("expected CommitRecoveryFailed, got {other:?}"),
    }
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn commit_409_budget_exceeded_with_retry_after_is_not_retried() {
    let server = MockServer::start().await;
    let client = test_client(&server);
    mount_reserve_allow(&server, "rsv_409be").await;
    mount_extend(&server, "rsv_409be").await;

    // A 409 BUDGET_EXCEEDED carrying a Retry-After header: the delay is
    // surfaced to the caller but must NOT make the error retryable — the
    // denial is a budget fact. expect(1) fails the test if a retry fires.
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_409be/commit"))
        .respond_with(
            ResponseTemplate::new(409)
                .insert_header("Retry-After", "5")
                .set_body_json(json!({
                    "error": "BUDGET_EXCEEDED",
                    "message": "Budget exceeded",
                    "request_id": "req-409be"
                })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let guard = reserve(&client).await;
    let err = guard
        .commit(
            CommitRequest::builder()
                .actual(Amount::usd_microcents(9000))
                .build(),
        )
        .await
        .unwrap_err();

    assert!(err.is_budget_exceeded());
    assert_eq!(err.retry_after(), Some(Duration::from_secs(5)));
    assert_eq!(err.status(), Some(409));
    assert!(
        !err.is_retryable(),
        "409 BUDGET_EXCEEDED must not be retryable even with Retry-After"
    );
    assert_eq!(
        calls_to(&server, "/commit").await.len(),
        1,
        "no retry may fire on a 409 BUDGET_EXCEEDED"
    );
    assert!(
        calls_to(&server, "/events").await.is_empty(),
        "BUDGET_EXCEEDED must not trigger the event fallback"
    );
}

#[tokio::test]
async fn retry_after_header_is_parsed_into_the_error() {
    let server = MockServer::start().await;
    let client = CyclesClient::builder("key", server.uri())
        .retry_enabled(false)
        .build();

    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_hdr/commit"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "7")
                .set_body_json(json!({
                    "error": "LIMIT_EXCEEDED",
                    "message": "Rate limited",
                    "request_id": "req-hdr"
                })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let err = client
        .commit_reservation(
            &ReservationId::new("rsv_hdr"),
            &CommitRequest::builder()
                .actual(Amount::usd_microcents(100))
                .build(),
        )
        .await
        .unwrap_err();

    assert_eq!(err.error_code(), Some(ErrorCode::LimitExceeded));
    assert!(err.is_retryable());
    assert_eq!(err.retry_after(), Some(Duration::from_secs(7)));
}
