//! Tests for model serialization matching the OpenAPI spec wire format.

use runcycles::models::*;
use serde_json::json;

// ─── Wire format compliance tests ────────────────────────────────

#[test]
fn reservation_create_request_wire_format() {
    let req = ReservationCreateRequest::builder()
        .idempotency_key(IdempotencyKey::new("test-key-1"))
        .subject(Subject {
            tenant: Some("acme".into()),
            workspace: Some("prod".into()),
            ..Default::default()
        })
        .action(Action {
            kind: "llm.completion".into(),
            name: "gpt-4o".into(),
            tags: Some(vec!["prod".into()]),
        })
        .estimate(Amount::usd_microcents(5000))
        .ttl_ms(30_000_u64)
        .overage_policy(CommitOveragePolicy::AllowIfAvailable)
        .dry_run(true)
        .build();

    let json = serde_json::to_value(&req).unwrap();

    // Verify snake_case field names (wire format)
    assert_eq!(json["idempotency_key"], "test-key-1");
    assert_eq!(json["subject"]["tenant"], "acme");
    assert_eq!(json["subject"]["workspace"], "prod");
    assert!(json["subject"].get("app").is_none()); // None fields skipped
    assert_eq!(json["action"]["kind"], "llm.completion");
    assert_eq!(json["action"]["name"], "gpt-4o");
    assert_eq!(json["action"]["tags"], json!(["prod"]));
    assert_eq!(json["estimate"]["unit"], "USD_MICROCENTS");
    assert_eq!(json["estimate"]["amount"], 5000);
    assert_eq!(json["ttl_ms"], 30_000);
    assert_eq!(json["overage_policy"], "ALLOW_IF_AVAILABLE");
    assert_eq!(json["dry_run"], true);
}

#[test]
fn reservation_create_request_defaults_omit_optional() {
    let req = ReservationCreateRequest::builder()
        .idempotency_key(IdempotencyKey::new("k"))
        .subject(Subject { tenant: Some("t".into()), ..Default::default() })
        .action(Action::new("a", "b"))
        .estimate(Amount::tokens(100))
        .build();

    let json = serde_json::to_value(&req).unwrap();
    assert!(json.get("grace_period_ms").is_none());
    assert!(json.get("overage_policy").is_none());
    assert!(json.get("dry_run").is_none()); // false is skipped
    assert!(json.get("metadata").is_none());
    // Default TTL is also skipped
    assert!(json.get("ttl_ms").is_none());
}

#[test]
fn commit_request_wire_format() {
    let req = CommitRequest::builder()
        .idempotency_key(IdempotencyKey::new("ck-1"))
        .actual(Amount::usd_microcents(3200))
        .metrics(CyclesMetrics {
            tokens_input: Some(100),
            tokens_output: Some(200),
            latency_ms: Some(500),
            model_version: Some("v1".into()),
            ..Default::default()
        })
        .build();

    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["idempotency_key"], "ck-1");
    assert_eq!(json["actual"]["unit"], "USD_MICROCENTS");
    assert_eq!(json["actual"]["amount"], 3200);
    assert_eq!(json["metrics"]["tokens_input"], 100);
    assert_eq!(json["metrics"]["tokens_output"], 200);
    assert_eq!(json["metrics"]["latency_ms"], 500);
    assert_eq!(json["metrics"]["model_version"], "v1");
}

#[test]
fn release_request_wire_format() {
    let req = ReleaseRequest::new(Some("user_cancelled".into()));

    let json = serde_json::to_value(&req).unwrap();
    assert!(json["idempotency_key"].is_string());
    assert_eq!(json["reason"], "user_cancelled");
}

#[test]
fn release_request_no_reason() {
    let req = ReleaseRequest::new(None);

    let json = serde_json::to_value(&req).unwrap();
    assert!(json.get("reason").is_none());
}

#[test]
fn extend_request_wire_format() {
    let req = ExtendRequest::new(60_000);

    let json = serde_json::to_value(&req).unwrap();
    assert!(json["idempotency_key"].is_string());
    assert_eq!(json["extend_by_ms"], 60_000);
    assert!(json.get("metadata").is_none());
}

#[test]
fn decision_request_wire_format() {
    let req = DecisionRequest::builder()
        .idempotency_key(IdempotencyKey::new("dk-1"))
        .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .build();

    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["idempotency_key"], "dk-1");
    assert_eq!(json["subject"]["tenant"], "acme");
    assert_eq!(json["action"]["kind"], "llm.completion");
    assert_eq!(json["estimate"]["unit"], "USD_MICROCENTS");
}

#[test]
fn event_create_request_wire_format() {
    let req = EventCreateRequest::builder()
        .idempotency_key(IdempotencyKey::new("ek-1"))
        .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
        .action(Action::new("tool.search", "web_search"))
        .actual(Amount::usd_microcents(1500))
        .overage_policy(CommitOveragePolicy::AllowWithOverdraft)
        .client_time_ms(1700000000000_u64)
        .build();

    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["idempotency_key"], "ek-1");
    assert_eq!(json["actual"]["amount"], 1500);
    assert_eq!(json["overage_policy"], "ALLOW_WITH_OVERDRAFT");
    assert_eq!(json["client_time_ms"], 1700000000000_u64);
}

// ─── Response deserialization tests ──────────────────────────────

#[test]
fn reservation_create_response_from_json() {
    let json = json!({
        "decision": "ALLOW_WITH_CAPS",
        "reservation_id": "rsv_abc",
        "affected_scopes": ["tenant:acme", "app:my-app"],
        "expires_at_ms": 1700000060000_u64,
        "scope_path": "tenant:acme/app:my-app",
        "reserved": {"unit": "TOKENS", "amount": 1000},
        "caps": {
            "max_tokens": 500,
            "tool_denylist": ["dangerous_tool"]
        },
        "balances": [
            {
                "scope": "tenant:acme",
                "scope_path": "tenant:acme",
                "remaining": {"unit": "TOKENS", "amount": 9000}
            }
        ]
    });

    let resp: ReservationCreateResponse = serde_json::from_value(json).unwrap();
    assert_eq!(resp.decision, Decision::AllowWithCaps);
    assert_eq!(resp.reservation_id.unwrap().as_str(), "rsv_abc");
    assert_eq!(resp.affected_scopes.len(), 2);
    assert_eq!(resp.expires_at_ms, Some(1700000060000));
    assert_eq!(resp.scope_path.as_deref(), Some("tenant:acme/app:my-app"));
    assert_eq!(resp.reserved.unwrap().amount, 1000);

    let caps = resp.caps.unwrap();
    assert_eq!(caps.max_tokens, Some(500));
    assert!(!caps.is_tool_allowed("dangerous_tool"));
    assert!(caps.is_tool_allowed("safe_tool"));

    assert_eq!(resp.balances.unwrap().len(), 1);
}

#[test]
fn commit_response_from_json() {
    let json = json!({
        "status": "COMMITTED",
        "charged": {"unit": "USD_MICROCENTS", "amount": 3200},
        "released": {"unit": "USD_MICROCENTS", "amount": 1800},
        "balances": []
    });

    let resp: CommitResponse = serde_json::from_value(json).unwrap();
    assert_eq!(resp.status, CommitStatus::Committed);
    assert_eq!(resp.charged.amount, 3200);
    assert_eq!(resp.released.unwrap().amount, 1800);
}

#[test]
fn event_create_response_from_json() {
    let json = json!({
        "status": "APPLIED",
        "event_id": "evt_xyz"
    });

    let resp: EventCreateResponse = serde_json::from_value(json).unwrap();
    assert_eq!(resp.status, EventStatus::Applied);
    assert_eq!(resp.event_id.as_str(), "evt_xyz");
    assert!(resp.charged.is_none());
}

#[test]
fn reservation_detail_from_json() {
    let json = json!({
        "reservation_id": "rsv_det",
        "status": "ACTIVE",
        "subject": {"tenant": "acme", "agent": "bot-1"},
        "action": {"kind": "llm.completion", "name": "gpt-4o"},
        "reserved": {"unit": "USD_MICROCENTS", "amount": 5000},
        "created_at_ms": 1700000000000_u64,
        "expires_at_ms": 1700000060000_u64,
        "scope_path": "tenant:acme/agent:bot-1",
        "affected_scopes": ["tenant:acme", "agent:bot-1"],
        "idempotency_key": "ik-1",
        "metadata": {"source": "test"}
    });

    let resp: ReservationDetail = serde_json::from_value(json).unwrap();
    assert_eq!(resp.reservation_id.as_str(), "rsv_det");
    assert_eq!(resp.status, ReservationStatus::Active);
    assert_eq!(resp.subject.tenant.as_deref(), Some("acme"));
    assert_eq!(resp.subject.agent.as_deref(), Some("bot-1"));
    assert_eq!(resp.reserved.amount, 5000);
    assert_eq!(resp.idempotency_key.as_deref(), Some("ik-1"));
    assert!(resp.committed.is_none());
}

#[test]
fn balance_with_debt_and_overdraft() {
    let json = json!({
        "scope": "tenant:acme",
        "scope_path": "tenant:acme",
        "remaining": {"unit": "USD_MICROCENTS", "amount": -5000},
        "reserved": {"unit": "USD_MICROCENTS", "amount": 0},
        "spent": {"unit": "USD_MICROCENTS", "amount": 50000},
        "allocated": {"unit": "USD_MICROCENTS", "amount": 45000},
        "debt": {"unit": "USD_MICROCENTS", "amount": 5000},
        "overdraft_limit": {"unit": "USD_MICROCENTS", "amount": 10000},
        "is_over_limit": false
    });

    let balance: Balance = serde_json::from_value(json).unwrap();
    assert_eq!(balance.remaining.amount, -5000); // SignedAmount can be negative
    assert_eq!(balance.debt.unwrap().amount, 5000);
    assert_eq!(balance.overdraft_limit.unwrap().amount, 10000);
    assert_eq!(balance.is_over_limit, Some(false));
}

#[test]
fn error_response_from_json() {
    let json = json!({
        "error": "BUDGET_EXCEEDED",
        "message": "Insufficient budget",
        "request_id": "req-123",
        "details": {"scope": "tenant:acme", "remaining": 100}
    });

    let resp: ErrorResponse = serde_json::from_value(json).unwrap();
    assert_eq!(resp.error, "BUDGET_EXCEEDED");
    assert_eq!(resp.message, "Insufficient budget");
    assert_eq!(resp.request_id.as_deref(), Some("req-123"));
    assert!(resp.details.is_some());
}

#[test]
fn unknown_future_fields_ignored() {
    // Server may add new fields; our #[non_exhaustive] structs should tolerate them
    let json = json!({
        "decision": "ALLOW",
        "reservation_id": "rsv_new",
        "affected_scopes": [],
        "new_future_field": "some_value",
        "another_field": 42
    });

    // This should not fail deserialization
    let resp: ReservationCreateResponse = serde_json::from_value(json).unwrap();
    assert_eq!(resp.decision, Decision::Allow);
}

// ─── Subject with dimensions ─────────────────────────────────────

#[test]
fn subject_with_dimensions_serializes() {
    let mut dims = std::collections::HashMap::new();
    dims.insert("cost_center".into(), "engineering".into());
    dims.insert("project".into(), "alpha".into());

    let subject = Subject {
        tenant: Some("acme".into()),
        dimensions: Some(dims),
        ..Default::default()
    };

    let json = serde_json::to_value(&subject).unwrap();
    assert_eq!(json["tenant"], "acme");
    assert_eq!(json["dimensions"]["cost_center"], "engineering");
    assert_eq!(json["dimensions"]["project"], "alpha");
}

// ─── BalanceParams ───────────────────────────────────────────────

#[test]
fn balance_params_has_filter() {
    let empty = BalanceParams::default();
    assert!(!empty.has_filter());

    let with_tenant = BalanceParams {
        tenant: Some("acme".into()),
        ..Default::default()
    };
    assert!(with_tenant.has_filter());

    let with_toolset = BalanceParams {
        toolset: Some("tools".into()),
        ..Default::default()
    };
    assert!(with_toolset.has_filter());
}

// ─── Dry run result ──────────────────────────────────────────────

#[test]
fn dry_run_result_from_json() {
    let json = json!({
        "decision": "ALLOW_WITH_CAPS",
        "caps": {"max_tokens": 200},
        "affected_scopes": ["tenant:acme"],
        "scope_path": "tenant:acme",
        "reserved": {"unit": "USD_MICROCENTS", "amount": 5000}
    });

    let result: DryRunResult = serde_json::from_value(json).unwrap();
    assert_eq!(result.decision, Decision::AllowWithCaps);
    assert_eq!(result.caps.unwrap().max_tokens, Some(200));
}
