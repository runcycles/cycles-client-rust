//! End-to-end tests for inline commit retry through the guard path.
//!
//! `ReservationGuard::commit` retries retryable failures inline (see
//! `CommitRetryEngine`), so by the time `commit()` returns, all commit
//! traffic has already reached the server — no background task exists.
//! These tests drive the public API — reserve → commit — and assert on the
//! requests the mock server received.

mod common;

use std::time::Duration;

use common::{make_reserve_request, mount_extend, mount_reserve_allow};
use runcycles::models::*;
use runcycles::{CyclesClient, ReservationGuard};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_client(server: &MockServer, retry_enabled: bool) -> CyclesClient {
    CyclesClient::builder("key", server.uri())
        .retry_enabled(retry_enabled)
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

fn commit_request() -> CommitRequest {
    CommitRequest::builder()
        .actual(Amount::usd_microcents(5000))
        .build()
}

/// Commit requests the server has received so far.
async fn commit_calls(server: &MockServer) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().ends_with("/commit"))
        .collect()
}

/// Regression tripwire: with the 10ms/×2 schedule above, all three configured
/// retry attempts fire within ~70ms, so any stray attempt a regression
/// (re)introduces — e.g. a detached background retry — lands well inside this
/// window and is caught by the post-settle recount plus the mocks' `.expect`
/// bounds, which wiremock verifies when the server drops.
const SETTLE: Duration = Duration::from_millis(300);

#[tokio::test]
async fn commit_retries_inline_until_success() {
    let server = MockServer::start().await;
    let client = test_client(&server, true);
    mount_reserve_allow(&server, "rsv_retry").await;
    mount_extend(&server, "rsv_retry").await;

    // First commit attempt fails with a retryable 500, the retry succeeds.
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_retry/commit"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "INTERNAL_ERROR",
            "message": "Temporary failure",
            "request_id": "req-retry"
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_retry/commit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "COMMITTED",
            "charged": {"unit": "USD_MICROCENTS", "amount": 5000}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let guard = reserve(&client).await;
    let resp = guard
        .commit(commit_request())
        .await
        .expect("inline retry should salvage the commit");
    assert_eq!(resp.status, CommitStatus::Committed);

    // Inline semantics: both requests arrived before commit() returned.
    let commits = commit_calls(&server).await;
    assert_eq!(commits.len(), 2, "expected exactly initial attempt + retry");

    // The retry reuses the same request, hence the same idempotency key —
    // a commit that landed server-side cannot double-charge.
    let keys: Vec<_> = commits
        .iter()
        .map(|r| {
            r.headers
                .get("X-Idempotency-Key")
                .expect("commit must carry idempotency key")
                .clone()
        })
        .collect();
    assert_eq!(keys[0], keys[1], "retry must reuse the idempotency key");

    // A retry-after-success regression would keep sending commits: the
    // recount below and the 200 mock's .expect(1) (verified on drop) catch it.
    tokio::time::sleep(SETTLE).await;
    assert_eq!(commit_calls(&server).await.len(), 2);
}

#[tokio::test]
async fn commit_retry_exhaustion_returns_last_error() {
    let server = MockServer::start().await;
    let client = test_client(&server, true);
    mount_reserve_allow(&server, "rsv_ex").await;
    mount_extend(&server, "rsv_ex").await;

    // Initial attempt + retry_max_attempts(3) retries, all failing.
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_ex/commit"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "INTERNAL_ERROR",
            "message": "Persistent failure",
            "request_id": "req-ex"
        })))
        .expect(4)
        .mount(&server)
        .await;

    let guard = reserve(&client).await;
    let err = guard.commit(commit_request()).await.unwrap_err();
    assert!(err.is_retryable());
    assert_eq!(commit_calls(&server).await.len(), 4);
}

#[tokio::test]
async fn commit_non_retryable_error_is_final_after_one_attempt() {
    let server = MockServer::start().await;
    let client = test_client(&server, true);
    mount_reserve_allow(&server, "rsv_nr").await;
    mount_extend(&server, "rsv_nr").await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_nr/commit"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "INVALID_REQUEST",
            "message": "Bad request",
            "request_id": "req-nr"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let guard = reserve(&client).await;
    let err = guard.commit(commit_request()).await.unwrap_err();
    assert!(!err.is_retryable());

    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        commit_calls(&server).await.len(),
        1,
        "non-retryable errors must not be retried"
    );
}

#[tokio::test]
async fn commit_with_retry_disabled_is_final_after_one_attempt() {
    let server = MockServer::start().await;
    let client = test_client(&server, false);
    mount_reserve_allow(&server, "rsv_off").await;
    mount_extend(&server, "rsv_off").await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_off/commit"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "INTERNAL_ERROR",
            "message": "Temporary failure",
            "request_id": "req-off"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let guard = reserve(&client).await;
    let err = guard.commit(commit_request()).await.unwrap_err();
    assert!(err.is_retryable());

    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        commit_calls(&server).await.len(),
        1,
        "retry_enabled=false must mean a single attempt"
    );
}
