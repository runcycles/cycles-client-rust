//! Tests for with_cycles() automatic lifecycle wrapper.

use runcycles::models::*;
use runcycles::{with_cycles, CyclesClient, Error, WithCyclesConfig};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup() -> (MockServer, CyclesClient) {
    let server = MockServer::start().await;
    let client = CyclesClient::builder("test-key", &server.uri()).build();
    (server, client)
}

fn mock_reserve(rsv_id: &str) -> Mock {
    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "ALLOW",
            "reservation_id": rsv_id,
            "affected_scopes": ["tenant:acme"]
        })))
}

fn mock_commit(rsv_id: &str) -> Mock {
    Mock::given(method("POST"))
        .and(path(format!("/v1/reservations/{rsv_id}/commit")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "COMMITTED",
            "charged": {"unit": "TOKENS", "amount": 42}
        })))
}

fn mock_release(rsv_id: &str) -> Mock {
    Mock::given(method("POST"))
        .and(path(format!("/v1/reservations/{rsv_id}/release")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "RELEASED",
            "released": {"unit": "TOKENS", "amount": 1000}
        })))
}

fn mock_extend(rsv_id: &str) -> Mock {
    Mock::given(method("POST"))
        .and(path(format!("/v1/reservations/{rsv_id}/extend")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ACTIVE",
            "expires_at_ms": 9999999999999_u64
        })))
}

#[tokio::test]
async fn with_cycles_success_commits_automatically() {
    let (server, client) = setup().await;

    mock_reserve("rsv_wc1").mount(&server).await;
    mock_commit("rsv_wc1").expect(1).mount(&server).await;
    mock_extend("rsv_wc1").mount(&server).await;

    let result = with_cycles(
        &client,
        WithCyclesConfig::new(Amount::tokens(1000))
            .action("llm.completion", "gpt-4o")
            .subject(Subject {
                tenant: Some("acme".into()),
                ..Default::default()
            }),
        |ctx| async move {
            assert_eq!(ctx.decision, Decision::Allow);
            let response = "Hello from LLM".to_string();
            Ok((response, Amount::tokens(42)))
        },
    )
    .await
    .unwrap();

    assert_eq!(result, "Hello from LLM");
}

#[tokio::test]
async fn with_cycles_error_releases_automatically() {
    let (server, client) = setup().await;

    mock_reserve("rsv_wc2").mount(&server).await;
    mock_release("rsv_wc2").expect(1).mount(&server).await;
    mock_extend("rsv_wc2").mount(&server).await;

    let err: Result<String, Error> = with_cycles(
        &client,
        WithCyclesConfig::new(Amount::tokens(1000))
            .action("llm.completion", "gpt-4o")
            .subject(Subject {
                tenant: Some("acme".into()),
                ..Default::default()
            }),
        |_ctx| async move {
            let e: Box<dyn std::error::Error + Send + Sync> = "LLM call failed".into();
            Err(e)
        },
    )
    .await;

    let err = err.unwrap_err();
    match err {
        Error::Validation(msg) => assert!(msg.contains("guarded function failed")),
        _ => panic!("expected Validation error, got: {:?}", err),
    }
}

#[tokio::test]
async fn with_cycles_budget_exceeded_returns_error() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "DENY",
            "affected_scopes": ["tenant:acme"],
            "reason_code": "BUDGET_EXCEEDED"
        })))
        .mount(&server)
        .await;

    let err: Result<String, Error> = with_cycles(
        &client,
        WithCyclesConfig::new(Amount::tokens(999999))
            .action("llm.completion", "gpt-4o")
            .subject(Subject {
                tenant: Some("acme".into()),
                ..Default::default()
            }),
        |_ctx| async move {
            panic!("should not be called");
        },
    )
    .await;

    assert!(err.unwrap_err().is_budget_exceeded());
}

#[tokio::test]
async fn with_cycles_caps_accessible_in_closure() {
    let (server, client) = setup().await;

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "ALLOW_WITH_CAPS",
            "reservation_id": "rsv_wc3",
            "affected_scopes": ["tenant:acme"],
            "caps": {"max_tokens": 200}
        })))
        .mount(&server)
        .await;
    mock_commit("rsv_wc3").mount(&server).await;
    mock_extend("rsv_wc3").mount(&server).await;

    let result = with_cycles(
        &client,
        WithCyclesConfig::new(Amount::tokens(1000))
            .action("llm.completion", "gpt-4o")
            .subject(Subject {
                tenant: Some("acme".into()),
                ..Default::default()
            }),
        |ctx| async move {
            assert_eq!(ctx.decision, Decision::AllowWithCaps);
            let caps = ctx.caps.as_ref().expect("should have caps");
            let max_tokens = caps.max_tokens.unwrap_or(1000);
            assert_eq!(max_tokens, 200);
            Ok(("capped response".to_string(), Amount::tokens(max_tokens)))
        },
    )
    .await
    .unwrap();

    assert_eq!(result, "capped response");
}

#[tokio::test]
async fn with_cycles_config_builder_all_fields() {
    let cfg = WithCyclesConfig::new(Amount::usd_microcents(5000))
        .action("tool.search", "web_search")
        .subject(Subject {
            tenant: Some("acme".into()),
            app: Some("my-app".into()),
            ..Default::default()
        })
        .ttl_ms(30_000)
        .grace_period_ms(5_000)
        .overage_policy(CommitOveragePolicy::AllowWithOverdraft)
        .action_tags(vec!["prod".into()])
        .metrics(CyclesMetrics {
            tokens_input: Some(100),
            tokens_output: Some(200),
            ..Default::default()
        });

    // Just verify it compiles and builds without panic
    let _ = cfg;
}
