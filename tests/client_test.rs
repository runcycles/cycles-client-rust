//! Integration tests for CyclesClient using wiremock.

use runcycles::models::*;
use runcycles::{CyclesClient, Error};
use serde_json::json;
use wiremock::matchers::{header, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup() -> (MockServer, CyclesClient) {
    let server = MockServer::start().await;
    let client = CyclesClient::builder("test-api-key", &server.uri()).build();
    (server, client)
}

// ─── create_reservation ───────────────────────────────────────────

#[tokio::test]
async fn create_reservation_success() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .and(header("X-Cycles-API-Key", "test-api-key"))
        .and(header("Content-Type", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "ALLOW",
            "reservation_id": "rsv_123",
            "affected_scopes": ["tenant:acme"],
            "expires_at_ms": 1700000000000_u64,
            "scope_path": "tenant:acme",
            "reserved": {"unit": "USD_MICROCENTS", "amount": 5000},
            "caps": null,
            "reason_code": null,
            "retry_after_ms": null,
            "balances": null
        })))
        .expect(1)
        .mount(&server)
        .await;

    let req = ReservationCreateRequest::builder()
        .subject(Subject {
            tenant: Some("acme".into()),
            ..Default::default()
        })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .build();

    let resp = client.create_reservation(&req).await.unwrap();
    assert_eq!(resp.decision, Decision::Allow);
    assert_eq!(resp.reservation_id.unwrap().as_str(), "rsv_123");
    assert_eq!(resp.affected_scopes, vec!["tenant:acme"]);
}

#[tokio::test]
async fn create_reservation_allow_with_caps() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "ALLOW_WITH_CAPS",
            "reservation_id": "rsv_456",
            "affected_scopes": ["tenant:acme"],
            "caps": {
                "max_tokens": 100,
                "tool_allowlist": ["web_search"]
            }
        })))
        .mount(&server)
        .await;

    let req = ReservationCreateRequest::builder()
        .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .build();

    let resp = client.create_reservation(&req).await.unwrap();
    assert_eq!(resp.decision, Decision::AllowWithCaps);
    let caps = resp.caps.unwrap();
    assert_eq!(caps.max_tokens, Some(100));
    assert!(caps.is_tool_allowed("web_search"));
    assert!(!caps.is_tool_allowed("code_exec"));
}

#[tokio::test]
async fn create_reservation_with_metadata_has_headers() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "decision": "ALLOW",
                    "reservation_id": "rsv_789",
                    "affected_scopes": []
                }))
                .append_header("x-request-id", "req-abc-123")
                .append_header("x-ratelimit-remaining", "99")
                .append_header("x-ratelimit-reset", "1700000000")
                .append_header("x-cycles-tenant", "acme"),
        )
        .mount(&server)
        .await;

    let req = ReservationCreateRequest::builder()
        .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .build();

    let resp = client.create_reservation_with_metadata(&req).await.unwrap();
    assert_eq!(resp.request_id.as_deref(), Some("req-abc-123"));
    assert_eq!(resp.rate_limit_remaining, Some(99));
    assert_eq!(resp.rate_limit_reset, Some(1700000000));
    assert_eq!(resp.cycles_tenant.as_deref(), Some("acme"));
    assert_eq!(resp.data.decision, Decision::Allow);

    // Deref works
    assert_eq!(resp.decision, Decision::Allow);
}

#[tokio::test]
async fn create_reservation_budget_exceeded() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "BUDGET_EXCEEDED",
            "message": "Insufficient budget for tenant:acme",
            "request_id": "req-err-1"
        })))
        .mount(&server)
        .await;

    let req = ReservationCreateRequest::builder()
        .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(999999))
        .build();

    let err = client.create_reservation(&req).await.unwrap_err();
    assert!(err.is_budget_exceeded());
    assert_eq!(err.request_id(), Some("req-err-1"));
}

#[tokio::test]
async fn create_reservation_server_error() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "INTERNAL_ERROR",
            "message": "Something went wrong",
            "request_id": "req-500"
        })))
        .mount(&server)
        .await;

    let req = ReservationCreateRequest::builder()
        .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .build();

    let err = client.create_reservation(&req).await.unwrap_err();
    assert!(err.is_retryable());
    assert_eq!(err.error_code(), Some(ErrorCode::InternalError));
}

// ─── commit_reservation ───────────────────────────────────────────

#[tokio::test]
async fn commit_reservation_success() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_123/commit"))
        .and(header("X-Cycles-API-Key", "test-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "COMMITTED",
            "charged": {"unit": "USD_MICROCENTS", "amount": 3200},
            "released": {"unit": "USD_MICROCENTS", "amount": 1800}
        })))
        .mount(&server)
        .await;

    let id = ReservationId::new("rsv_123");
    let req = CommitRequest::builder()
        .actual(Amount::usd_microcents(3200))
        .build();

    let resp = client.commit_reservation(&id, &req).await.unwrap();
    assert_eq!(resp.status, CommitStatus::Committed);
    assert_eq!(resp.charged.amount, 3200);
    assert_eq!(resp.released.unwrap().amount, 1800);
}

#[tokio::test]
async fn commit_with_metrics() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_m/commit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "COMMITTED",
            "charged": {"unit": "TOKENS", "amount": 300}
        })))
        .mount(&server)
        .await;

    let id = ReservationId::new("rsv_m");
    let req = CommitRequest::builder()
        .actual(Amount::tokens(300))
        .metrics(CyclesMetrics {
            tokens_input: Some(100),
            tokens_output: Some(200),
            latency_ms: Some(1500),
            model_version: Some("gpt-4o-2024-05".to_string()),
            ..Default::default()
        })
        .build();

    let resp = client.commit_reservation(&id, &req).await.unwrap();
    assert_eq!(resp.charged.unit, Unit::Tokens);
    assert_eq!(resp.charged.amount, 300);
}

// ─── release_reservation ──────────────────────────────────────────

#[tokio::test]
async fn release_reservation_success() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_rel/release"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "RELEASED",
            "released": {"unit": "USD_MICROCENTS", "amount": 5000}
        })))
        .mount(&server)
        .await;

    let id = ReservationId::new("rsv_rel");
    let req = ReleaseRequest::new(Some("user_cancelled".to_string()));

    let resp = client.release_reservation(&id, &req).await.unwrap();
    assert_eq!(resp.status, ReleaseStatus::Released);
    assert_eq!(resp.released.amount, 5000);
}

// ─── extend_reservation ──────────────────────────────────────────

#[tokio::test]
async fn extend_reservation_success() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_ext/extend"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ACTIVE",
            "expires_at_ms": 1700000060000_u64
        })))
        .mount(&server)
        .await;

    let id = ReservationId::new("rsv_ext");
    let req = ExtendRequest::new(60_000);

    let resp = client.extend_reservation(&id, &req).await.unwrap();
    assert_eq!(resp.status, ExtendStatus::Active);
    assert_eq!(resp.expires_at_ms, 1700000060000);
}

// ─── decide ──────────────────────────────────────────────────────

#[tokio::test]
async fn decide_allow() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/decide"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "ALLOW"
        })))
        .mount(&server)
        .await;

    let req = DecisionRequest::builder()
        .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .build();

    let resp = client.decide(&req).await.unwrap();
    assert_eq!(resp.decision, Decision::Allow);
    assert!(resp.caps.is_none());
}

#[tokio::test]
async fn decide_deny() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/decide"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "DENY",
            "reason_code": "DEBT_OUTSTANDING",
            "retry_after_ms": 5000
        })))
        .mount(&server)
        .await;

    let req = DecisionRequest::builder()
        .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .build();

    let resp = client.decide(&req).await.unwrap();
    assert!(resp.decision.is_denied());
    assert_eq!(resp.reason_code.as_deref(), Some("DEBT_OUTSTANDING"));
    assert_eq!(resp.retry_after_ms, Some(5000));
}

// ─── create_event ────────────────────────────────────────────────

#[tokio::test]
async fn create_event_success() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/events"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "status": "APPLIED",
            "event_id": "evt_001",
            "charged": {"unit": "USD_MICROCENTS", "amount": 1500}
        })))
        .mount(&server)
        .await;

    let req = EventCreateRequest::builder()
        .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
        .action(Action::new("tool.search", "web_search"))
        .actual(Amount::usd_microcents(1500))
        .build();

    let resp = client.create_event(&req).await.unwrap();
    assert_eq!(resp.status, EventStatus::Applied);
    assert_eq!(resp.event_id.as_str(), "evt_001");
    assert_eq!(resp.charged.unwrap().amount, 1500);
}

// ─── list_reservations ──────────────────────────────────────────

#[tokio::test]
async fn list_reservations_success() {
    let (server, client) = setup().await;

    Mock::given(method("GET"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "reservations": [
                {
                    "reservation_id": "rsv_1",
                    "status": "ACTIVE",
                    "subject": {"tenant": "acme"},
                    "action": {"kind": "llm.completion", "name": "gpt-4o"},
                    "reserved": {"unit": "USD_MICROCENTS", "amount": 5000},
                    "created_at_ms": 1700000000000_u64,
                    "expires_at_ms": 1700000060000_u64,
                    "scope_path": "tenant:acme",
                    "affected_scopes": ["tenant:acme"]
                }
            ],
            "has_more": false
        })))
        .mount(&server)
        .await;

    let params = ListReservationsParams::default();
    let resp = client.list_reservations(&params).await.unwrap();
    assert_eq!(resp.reservations.len(), 1);
    assert_eq!(resp.reservations[0].reservation_id.as_str(), "rsv_1");
    assert_eq!(resp.reservations[0].status, ReservationStatus::Active);
    assert_eq!(resp.has_more, Some(false));
}

// ─── get_reservation ─────────────────────────────────────────────

#[tokio::test]
async fn get_reservation_success() {
    let (server, client) = setup().await;

    Mock::given(method("GET"))
        .and(path("/v1/reservations/rsv_detail"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "reservation_id": "rsv_detail",
            "status": "COMMITTED",
            "subject": {"tenant": "acme"},
            "action": {"kind": "llm.completion", "name": "gpt-4o"},
            "reserved": {"unit": "USD_MICROCENTS", "amount": 5000},
            "committed": {"unit": "USD_MICROCENTS", "amount": 3200},
            "created_at_ms": 1700000000000_u64,
            "expires_at_ms": 1700000060000_u64,
            "finalized_at_ms": 1700000030000_u64,
            "scope_path": "tenant:acme",
            "affected_scopes": ["tenant:acme"]
        })))
        .mount(&server)
        .await;

    let id = ReservationId::new("rsv_detail");
    let resp = client.get_reservation(&id).await.unwrap();
    assert_eq!(resp.status, ReservationStatus::Committed);
    assert_eq!(resp.committed.unwrap().amount, 3200);
    assert_eq!(resp.finalized_at_ms, Some(1700000030000));
}

// ─── get_balances ────────────────────────────────────────────────

#[tokio::test]
async fn get_balances_success() {
    let (server, client) = setup().await;

    Mock::given(method("GET"))
        .and(path("/v1/balances"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "balances": [
                {
                    "scope": "tenant:acme",
                    "scope_path": "tenant:acme",
                    "remaining": {"unit": "USD_MICROCENTS", "amount": 50000},
                    "reserved": {"unit": "USD_MICROCENTS", "amount": 5000},
                    "spent": {"unit": "USD_MICROCENTS", "amount": 10000},
                    "allocated": {"unit": "USD_MICROCENTS", "amount": 65000},
                    "is_over_limit": false
                }
            ]
        })))
        .mount(&server)
        .await;

    let params = BalanceParams {
        tenant: Some("acme".into()),
        ..Default::default()
    };

    let resp = client.get_balances(&params).await.unwrap();
    assert_eq!(resp.balances.len(), 1);
    assert_eq!(resp.balances[0].scope, "tenant:acme");
    assert_eq!(resp.balances[0].remaining.amount, 50000);
    assert_eq!(resp.balances[0].is_over_limit, Some(false));
}

#[tokio::test]
async fn get_balances_requires_filter() {
    let (_server, client) = setup().await;
    let params = BalanceParams::default();
    let err = client.get_balances(&params).await.unwrap_err();
    match err {
        Error::Validation(msg) => {
            assert!(msg.contains("filter"));
        }
        _ => panic!("expected Validation error, got {:?}", err),
    }
}

// ─── reserve (high-level) ────────────────────────────────────────

#[tokio::test]
async fn reserve_returns_guard_on_allow() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "ALLOW",
            "reservation_id": "rsv_guard",
            "affected_scopes": ["tenant:acme"],
            "expires_at_ms": 1700000060000_u64
        })))
        .mount(&server)
        .await;

    // Also mock extend for heartbeat and release for guard drop
    Mock::given(method("POST"))
        .and(path_regex("/v1/reservations/rsv_guard/(extend|release)"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ACTIVE",
            "expires_at_ms": 1700000120000_u64
        })))
        .mount(&server)
        .await;

    let guard = client
        .reserve(
            ReservationCreateRequest::builder()
                .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::usd_microcents(5000))
                .build(),
        )
        .await
        .unwrap();

    assert_eq!(guard.reservation_id().as_str(), "rsv_guard");
    assert_eq!(guard.decision(), Decision::Allow);
    assert!(guard.caps().is_none());
    assert!(!guard.is_capped());
    assert_eq!(guard.affected_scopes(), &["tenant:acme"]);
    assert_eq!(guard.expires_at_ms(), Some(1700000060000));

    // Drop triggers release
    drop(guard);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}

#[tokio::test]
async fn reserve_returns_error_on_deny() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "DENY",
            "affected_scopes": ["tenant:acme"],
            "reason_code": "BUDGET_EXCEEDED",
            "retry_after_ms": 10000
        })))
        .mount(&server)
        .await;

    let err = client
        .reserve(
            ReservationCreateRequest::builder()
                .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::usd_microcents(999999))
                .build(),
        )
        .await
        .unwrap_err();

    assert!(err.is_budget_exceeded());
}

#[tokio::test]
async fn reserve_validates_subject() {
    let (_server, client) = setup().await;

    let err = client
        .reserve(
            ReservationCreateRequest::builder()
                .subject(Subject::default()) // no fields set
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::usd_microcents(5000))
                .build(),
        )
        .await
        .unwrap_err();

    match err {
        Error::Validation(msg) => assert!(msg.contains("Subject")),
        _ => panic!("expected Validation error"),
    }
}

#[tokio::test]
async fn reserve_validates_ttl() {
    let (_server, client) = setup().await;

    let err = client
        .reserve(
            ReservationCreateRequest::builder()
                .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::usd_microcents(5000))
                .ttl_ms(500_u64) // too low
                .build(),
        )
        .await
        .unwrap_err();

    match err {
        Error::Validation(msg) => assert!(msg.contains("ttl_ms")),
        _ => panic!("expected Validation error"),
    }
}

#[tokio::test]
async fn reserve_validates_negative_estimate() {
    let (_server, client) = setup().await;

    let err = client
        .reserve(
            ReservationCreateRequest::builder()
                .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::usd_microcents(-1))
                .build(),
        )
        .await
        .unwrap_err();

    match err {
        Error::Validation(msg) => assert!(msg.contains("non-negative")),
        _ => panic!("expected Validation error"),
    }
}

// ─── guard commit and release ────────────────────────────────────

#[tokio::test]
async fn guard_commit_success() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "ALLOW",
            "reservation_id": "rsv_gc",
            "affected_scopes": ["tenant:acme"]
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_gc/commit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "COMMITTED",
            "charged": {"unit": "USD_MICROCENTS", "amount": 3200}
        })))
        .mount(&server)
        .await;

    // Mock extend for heartbeat
    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_gc/extend"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ACTIVE",
            "expires_at_ms": 1700000120000_u64
        })))
        .mount(&server)
        .await;

    let guard = client
        .reserve(
            ReservationCreateRequest::builder()
                .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::usd_microcents(5000))
                .build(),
        )
        .await
        .unwrap();

    let resp = guard
        .commit(CommitRequest::builder().actual(Amount::usd_microcents(3200)).build())
        .await
        .unwrap();

    assert_eq!(resp.status, CommitStatus::Committed);
    assert_eq!(resp.charged.amount, 3200);
}

#[tokio::test]
async fn guard_release_success() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "ALLOW",
            "reservation_id": "rsv_gr",
            "affected_scopes": ["tenant:acme"]
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_gr/release"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "RELEASED",
            "released": {"unit": "USD_MICROCENTS", "amount": 5000}
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations/rsv_gr/extend"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ACTIVE",
            "expires_at_ms": 1700000120000_u64
        })))
        .mount(&server)
        .await;

    let guard = client
        .reserve(
            ReservationCreateRequest::builder()
                .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::usd_microcents(5000))
                .build(),
        )
        .await
        .unwrap();

    let resp = guard.release("user_cancelled").await.unwrap();
    assert_eq!(resp.status, ReleaseStatus::Released);
}

// ─── transport error ──────────────────────────────────────────────

#[tokio::test]
async fn transport_error_on_bad_url() {
    let client = CyclesClient::builder("test-key", "http://127.0.0.1:1")
        .connect_timeout(std::time::Duration::from_millis(100))
        .build();

    let req = ReservationCreateRequest::builder()
        .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .build();

    let err = client.create_reservation(&req).await.unwrap_err();
    assert!(matches!(err, Error::Transport(_)));
    assert!(err.is_retryable());
}

// ─── idempotency key header ──────────────────────────────────────

#[tokio::test]
async fn idempotency_key_sent_as_header() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .and(header("X-Idempotency-Key", "my-idem-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "ALLOW",
            "reservation_id": "rsv_idem",
            "affected_scopes": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let req = ReservationCreateRequest::builder()
        .idempotency_key(IdempotencyKey::new("my-idem-key"))
        .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .build();

    client.create_reservation(&req).await.unwrap();
}

// ─── unknown error code handling ─────────────────────────────────

#[tokio::test]
async fn unknown_error_code_does_not_crash() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "error": "SOME_FUTURE_ERROR",
            "message": "A new error type",
            "request_id": "req-future"
        })))
        .mount(&server)
        .await;

    let req = ReservationCreateRequest::builder()
        .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .build();

    let err = client.create_reservation(&req).await.unwrap_err();
    match err {
        Error::Api { code, message, .. } => {
            assert_eq!(code, Some(ErrorCode::Unknown));
            assert_eq!(message, "A new error type");
        }
        _ => panic!("expected Api error"),
    }
}
