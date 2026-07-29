//! Durable pending-commit journal and restart-replay tests.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{make_reserve_request, mount_extend, mount_reserve_allow};
use runcycles::models::*;
use runcycles::{CyclesClient, Error};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn durable_client(server: &MockServer, journal_dir: &Path, api_key: &str) -> CyclesClient {
    durable_client_with_attempts(server, journal_dir, api_key, 0)
}

fn durable_client_with_attempts(
    server: &MockServer,
    journal_dir: &Path,
    api_key: &str,
    attempts: u32,
) -> CyclesClient {
    CyclesClient::builder(api_key, server.uri())
        .tenant("acme")
        .journal_dir(journal_dir)
        .retry_enabled(true)
        .retry_max_attempts(attempts)
        .retry_initial_delay(Duration::ZERO)
        .build()
}

fn journal_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(identities) = fs::read_dir(root) else {
        return files;
    };
    for identity in identities.flatten() {
        let Ok(entries) = fs::read_dir(identity.path()) else {
            continue;
        };
        files.extend(
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "json")),
        );
    }
    files.sort();
    files
}

async fn wait_until_journal_empty(root: &Path) {
    for _ in 0..100 {
        if journal_files(root).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("journal did not drain before timeout");
}

async fn wait_until_commit_count(server: &MockServer, expected: usize) {
    for _ in 0..100 {
        let count = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path().ends_with("/commit"))
            .count();
        if count >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("commit request count did not reach {expected}");
}

fn commit_request() -> CommitRequest {
    CommitRequest::builder()
        .idempotency_key(IdempotencyKey::new("durable-key"))
        .actual(Amount::usd_microcents(4200))
        .metadata(json!({"trace": "durable"}))
        .build()
}

async fn reserve(client: &CyclesClient) -> runcycles::ReservationGuard {
    client.reserve(make_reserve_request()).await.unwrap()
}

#[tokio::test]
async fn transient_commit_is_journaled_and_replayed_after_key_rotation() {
    let server = MockServer::start().await;
    let journal_dir = tempfile::tempdir().unwrap();
    mount_reserve_allow(&server, "rsv_restart").await;
    mount_extend(&server, "rsv_restart").await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_restart/commit"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": "INTERNAL_ERROR",
            "message": "temporary",
            "request_id": "req-1"
        })))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_restart/commit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "COMMITTED",
            "charged": {"unit": "USD_MICROCENTS", "amount": 4200}
        })))
        .mount(&server)
        .await;

    let first = durable_client(&server, journal_dir.path(), "old-key");
    let error = reserve(&first)
        .await
        .commit(commit_request())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::CommitPending { .. }));
    let files = journal_files(journal_dir.path());
    assert_eq!(files.len(), 1);
    let body = fs::read_to_string(&files[0]).unwrap();
    assert!(!body.contains("old-key"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["mode"],
        "commit"
    );

    // Tenant identity, not the API key, owns the partition. A new credential
    // finds and settles the old record automatically.
    let _second = durable_client_with_attempts(&server, journal_dir.path(), "rotated-key", 1);
    wait_until_journal_empty(journal_dir.path()).await;

    let commits = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path().ends_with("/commit"))
        .collect::<Vec<_>>();
    assert_eq!(commits.len(), 3);
    assert_eq!(
        commits[0].headers.get("X-Idempotency-Key"),
        commits[1].headers.get("X-Idempotency-Key")
    );
    assert_eq!(
        commits[1].headers.get("X-Idempotency-Key"),
        commits[2].headers.get("X-Idempotency-Key")
    );
}

#[tokio::test]
async fn expired_commit_persists_event_mode_before_restart_replay() {
    let server = MockServer::start().await;
    let journal_dir = tempfile::tempdir().unwrap();
    mount_reserve_allow(&server, "rsv_event").await;
    mount_extend(&server, "rsv_event").await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_event/commit"))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({
            "error": "RESERVATION_EXPIRED",
            "message": "expired",
            "request_id": "req-expired"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": "INTERNAL_ERROR",
            "message": "temporary",
            "request_id": "req-event-1"
        })))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "status": "APPLIED",
            "event_id": "evt-replayed",
            "charged": {"unit": "USD_MICROCENTS", "amount": 4200}
        })))
        .mount(&server)
        .await;

    let first = durable_client(&server, journal_dir.path(), "key");
    let error = reserve(&first)
        .await
        .commit(commit_request())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::CommitPending { .. }));
    let files = journal_files(journal_dir.path());
    assert_eq!(files.len(), 1);
    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(record["mode"], "event");
    assert_eq!(
        record["event_fallback_body"]["metadata"]["recovered_reservation_id"],
        "rsv_event"
    );

    let _second = durable_client_with_attempts(&server, journal_dir.path(), "key", 1);
    wait_until_journal_empty(journal_dir.path()).await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path().ends_with("/commit"))
            .count(),
        1,
        "event-mode replay must not retry the expired commit"
    );
    let events = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/events")
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0].headers.get("X-Idempotency-Key"),
        events[1].headers.get("X-Idempotency-Key")
    );
    assert_eq!(
        events[1].headers.get("X-Idempotency-Key"),
        events[2].headers.get("X-Idempotency-Key")
    );
}

#[tokio::test]
async fn rate_limit_retry_floor_is_persisted() {
    let server = MockServer::start().await;
    let journal_dir = tempfile::tempdir().unwrap();
    mount_reserve_allow(&server, "rsv_limited").await;
    mount_extend(&server, "rsv_limited").await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_limited/commit"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .set_body_json(json!({
                    "error": "LIMIT_EXCEEDED",
                    "message": "slow down",
                    "request_id": "req-limited"
                })),
        )
        .mount(&server)
        .await;

    let before_ms: u64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap();
    let client = durable_client(&server, journal_dir.path(), "key");
    let error = reserve(&client)
        .await
        .commit(commit_request())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::CommitPending { .. }));

    let files = journal_files(journal_dir.path());
    assert_eq!(files.len(), 1);
    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert!(record["not_before_ms"].as_u64().unwrap() >= before_ms + 900);

    let started = std::time::Instant::now();
    assert_eq!(
        client
            .flush_pending_commits_with_timeout(Duration::from_millis(20))
            .await,
        0
    );
    assert!(started.elapsed() < Duration::from_millis(250));
    assert_eq!(journal_files(journal_dir.path()).len(), 1);
}

#[tokio::test]
async fn synchronous_success_and_terminal_rejection_remove_the_preflight_record() {
    let success_server = MockServer::start().await;
    let success_dir = tempfile::tempdir().unwrap();
    mount_reserve_allow(&success_server, "rsv_success").await;
    mount_extend(&success_server, "rsv_success").await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_success/commit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "COMMITTED",
            "charged": {"unit": "USD_MICROCENTS", "amount": 4200}
        })))
        .mount(&success_server)
        .await;
    let success_client = durable_client(&success_server, success_dir.path(), "key");
    reserve(&success_client)
        .await
        .commit(commit_request())
        .await
        .unwrap();
    assert!(journal_files(success_dir.path()).is_empty());

    let rejected_server = MockServer::start().await;
    let rejected_dir = tempfile::tempdir().unwrap();
    mount_reserve_allow(&rejected_server, "rsv_rejected").await;
    mount_extend(&rejected_server, "rsv_rejected").await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_rejected/commit"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "INVALID_REQUEST",
            "message": "bad commit",
            "request_id": "req-rejected"
        })))
        .mount(&rejected_server)
        .await;
    let rejected_client = durable_client(&rejected_server, rejected_dir.path(), "key");
    let error = reserve(&rejected_client)
        .await
        .commit(commit_request())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Api { status: 400, .. }));
    assert!(journal_files(rejected_dir.path()).is_empty());
}

#[tokio::test]
async fn retry_after_is_journaled_before_inline_commit_and_event_retries() {
    let commit_server = MockServer::start().await;
    let commit_dir = tempfile::tempdir().unwrap();
    mount_reserve_allow(&commit_server, "rsv_commit_retry").await;
    mount_extend(&commit_server, "rsv_commit_retry").await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_commit_retry/commit"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "0")
                .set_body_json(json!({
                    "error": "LIMIT_EXCEEDED",
                    "message": "retry",
                    "request_id": "req-retry"
                })),
        )
        .up_to_n_times(1)
        .mount(&commit_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_commit_retry/commit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "COMMITTED",
            "charged": {"unit": "USD_MICROCENTS", "amount": 4200}
        })))
        .mount(&commit_server)
        .await;
    let commit_client = durable_client_with_attempts(&commit_server, commit_dir.path(), "key", 1);
    reserve(&commit_client)
        .await
        .commit(commit_request())
        .await
        .unwrap();
    assert!(journal_files(commit_dir.path()).is_empty());

    let event_server = MockServer::start().await;
    let event_dir = tempfile::tempdir().unwrap();
    mount_reserve_allow(&event_server, "rsv_event_retry").await;
    mount_extend(&event_server, "rsv_event_retry").await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_event_retry/commit"))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({
            "error": "RESERVATION_EXPIRED",
            "message": "expired",
            "request_id": "req-expired"
        })))
        .mount(&event_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "0")
                .set_body_json(json!({
                    "error": "LIMIT_EXCEEDED",
                    "message": "retry",
                    "request_id": "req-event-retry"
                })),
        )
        .up_to_n_times(1)
        .mount(&event_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "status": "APPLIED",
            "event_id": "evt-retry",
            "charged": {"unit": "USD_MICROCENTS", "amount": 4200}
        })))
        .mount(&event_server)
        .await;
    let event_client = durable_client_with_attempts(&event_server, event_dir.path(), "key", 1);
    let response = reserve(&event_client)
        .await
        .commit(commit_request())
        .await
        .unwrap();
    assert_eq!(response.status, CommitStatus::RecoveredViaEvent);
    assert!(journal_files(event_dir.path()).is_empty());
}

#[tokio::test]
async fn replay_retains_ambiguous_auth_and_discards_understood_rejection() {
    let auth_server = MockServer::start().await;
    let auth_dir = tempfile::tempdir().unwrap();
    mount_reserve_allow(&auth_server, "rsv_auth").await;
    mount_extend(&auth_server, "rsv_auth").await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_auth/commit"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": "INTERNAL_ERROR",
            "message": "temporary",
            "request_id": "req-auth-first"
        })))
        .up_to_n_times(1)
        .mount(&auth_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_auth/commit"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "UNAUTHORIZED",
            "message": "rotate credentials",
            "request_id": "req-auth-second"
        })))
        .mount(&auth_server)
        .await;
    let first = durable_client(&auth_server, auth_dir.path(), "key");
    assert!(matches!(
        reserve(&first)
            .await
            .commit(commit_request())
            .await
            .unwrap_err(),
        Error::CommitPending { .. }
    ));
    let _second = durable_client(&auth_server, auth_dir.path(), "key");
    wait_until_commit_count(&auth_server, 2).await;
    assert_eq!(journal_files(auth_dir.path()).len(), 1);

    let terminal_server = MockServer::start().await;
    let terminal_dir = tempfile::tempdir().unwrap();
    mount_reserve_allow(&terminal_server, "rsv_terminal").await;
    mount_extend(&terminal_server, "rsv_terminal").await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_terminal/commit"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": "INTERNAL_ERROR",
            "message": "temporary",
            "request_id": "req-terminal-first"
        })))
        .up_to_n_times(1)
        .mount(&terminal_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_terminal/commit"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "INVALID_REQUEST",
            "message": "terminal",
            "request_id": "req-terminal-second"
        })))
        .mount(&terminal_server)
        .await;
    let first = durable_client(&terminal_server, terminal_dir.path(), "key");
    assert!(matches!(
        reserve(&first)
            .await
            .commit(commit_request())
            .await
            .unwrap_err(),
        Error::CommitPending { .. }
    ));
    let _second = durable_client(&terminal_server, terminal_dir.path(), "key");
    wait_until_journal_empty(terminal_dir.path()).await;
}

#[tokio::test]
async fn event_replay_retains_unknown_success_and_discards_terminal_rejection() {
    let unknown_server = MockServer::start().await;
    let unknown_dir = tempfile::tempdir().unwrap();
    mount_reserve_allow(&unknown_server, "rsv_unknown_event").await;
    mount_extend(&unknown_server, "rsv_unknown_event").await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_unknown_event/commit"))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({
            "error": "RESERVATION_EXPIRED",
            "message": "expired",
            "request_id": "req-unknown-expired"
        })))
        .mount(&unknown_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": "INTERNAL_ERROR",
            "message": "temporary",
            "request_id": "req-unknown-first"
        })))
        .up_to_n_times(1)
        .mount(&unknown_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "status": "FUTURE_STATUS",
            "event_id": "evt-unknown"
        })))
        .mount(&unknown_server)
        .await;
    let first = durable_client(&unknown_server, unknown_dir.path(), "key");
    assert!(matches!(
        reserve(&first)
            .await
            .commit(commit_request())
            .await
            .unwrap_err(),
        Error::CommitPending { .. }
    ));
    let _second = durable_client(&unknown_server, unknown_dir.path(), "key");
    for _ in 0..100 {
        if unknown_server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/v1/events")
            .count()
            >= 2
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(journal_files(unknown_dir.path()).len(), 1);

    let terminal_server = MockServer::start().await;
    let terminal_dir = tempfile::tempdir().unwrap();
    mount_reserve_allow(&terminal_server, "rsv_terminal_event").await;
    mount_extend(&terminal_server, "rsv_terminal_event").await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_terminal_event/commit"))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({
            "error": "RESERVATION_EXPIRED",
            "message": "expired",
            "request_id": "req-terminal-expired"
        })))
        .mount(&terminal_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": "INTERNAL_ERROR",
            "message": "temporary",
            "request_id": "req-terminal-event-first"
        })))
        .up_to_n_times(1)
        .mount(&terminal_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "INVALID_REQUEST",
            "message": "terminal",
            "request_id": "req-terminal-event-second"
        })))
        .mount(&terminal_server)
        .await;
    let first = durable_client(&terminal_server, terminal_dir.path(), "key");
    assert!(matches!(
        reserve(&first)
            .await
            .commit(commit_request())
            .await
            .unwrap_err(),
        Error::CommitPending { .. }
    ));
    let _second = durable_client(&terminal_server, terminal_dir.path(), "key");
    wait_until_journal_empty(terminal_dir.path()).await;
}

#[tokio::test]
async fn protocol_invalid_2xx_settlement_responses_remain_durably_ambiguous() {
    let commit_server = MockServer::start().await;
    let commit_dir = tempfile::tempdir().unwrap();
    mount_reserve_allow(&commit_server, "rsv_wrong_commit_status").await;
    mount_extend(&commit_server, "rsv_wrong_commit_status").await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_wrong_commit_status/commit"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "status": "COMMITTED",
            "charged": {"unit": "USD_MICROCENTS", "amount": 4200}
        })))
        .mount(&commit_server)
        .await;

    let client = durable_client_with_attempts(&commit_server, commit_dir.path(), "key", 1);
    assert!(matches!(
        reserve(&client)
            .await
            .commit(commit_request())
            .await
            .unwrap_err(),
        Error::CommitPending { .. }
    ));
    assert_eq!(journal_files(commit_dir.path()).len(), 1);
    let commits = commit_server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path().ends_with("/commit"))
        .collect::<Vec<_>>();
    assert_eq!(commits.len(), 2);
    assert_eq!(
        commits[0].headers.get("X-Idempotency-Key"),
        commits[1].headers.get("X-Idempotency-Key")
    );

    let event_server = MockServer::start().await;
    let event_dir = tempfile::tempdir().unwrap();
    mount_reserve_allow(&event_server, "rsv_wrong_event_status").await;
    mount_extend(&event_server, "rsv_wrong_event_status").await;
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_wrong_event_status/commit"))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({
            "error": "RESERVATION_EXPIRED",
            "message": "expired",
            "request_id": "req-wrong-event-status"
        })))
        .mount(&event_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "APPLIED",
            "event_id": "evt-wrong-status"
        })))
        .mount(&event_server)
        .await;

    let client = durable_client_with_attempts(&event_server, event_dir.path(), "key", 1);
    assert!(matches!(
        reserve(&client)
            .await
            .commit(commit_request())
            .await
            .unwrap_err(),
        Error::CommitPending { .. }
    ));
    let files = journal_files(event_dir.path());
    assert_eq!(files.len(), 1);
    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(record["mode"], "event");
    let events = event_server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/v1/events")
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].headers.get("X-Idempotency-Key"),
        events[1].headers.get("X-Idempotency-Key")
    );
}
