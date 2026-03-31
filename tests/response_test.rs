//! Tests for ApiResponse wrapper.

use runcycles::models::*;
use runcycles::CyclesClient;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn api_response_into_inner() {
    let server = MockServer::start().await;
    let client = CyclesClient::builder("key", server.uri()).build();

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "decision": "ALLOW",
                    "reservation_id": "rsv_inner",
                    "affected_scopes": ["tenant:acme"]
                }))
                .append_header("x-request-id", "req-inner")
                .append_header("x-cycles-tenant", "acme"),
        )
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

    let api_resp = client.create_reservation_with_metadata(&req).await.unwrap();

    // Test Deref
    assert_eq!(api_resp.decision, Decision::Allow);

    // Test metadata
    assert_eq!(api_resp.request_id.as_deref(), Some("req-inner"));
    assert_eq!(api_resp.cycles_tenant.as_deref(), Some("acme"));

    // Test into_inner
    let inner = api_resp.into_inner();
    assert_eq!(inner.decision, Decision::Allow);
    assert_eq!(inner.reservation_id.unwrap().as_str(), "rsv_inner");
}

#[tokio::test]
async fn api_response_missing_optional_headers() {
    let server = MockServer::start().await;
    let client = CyclesClient::builder("key", server.uri()).build();

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "ALLOW",
            "reservation_id": "rsv_no_headers",
            "affected_scopes": []
        })))
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

    let api_resp = client.create_reservation_with_metadata(&req).await.unwrap();
    assert!(api_resp.request_id.is_none());
    assert!(api_resp.rate_limit_remaining.is_none());
    assert!(api_resp.rate_limit_reset.is_none());
    assert!(api_resp.cycles_tenant.is_none());
}
