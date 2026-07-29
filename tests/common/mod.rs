//! Shared wiremock scaffolding for integration tests.
//!
//! Each integration-test crate compiles this module independently, so not
//! every crate uses every helper.
#![allow(dead_code)]

use runcycles::models::*;
use runcycles::CyclesClient;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The standard reserve request used across suites: tenant `acme`,
/// `llm.completion`/`gpt-4o`, 5000 USD microcents.
pub fn make_reserve_request() -> ReservationCreateRequest {
    ReservationCreateRequest::builder()
        .subject(Subject {
            tenant: Some("acme".into()),
            ..Default::default()
        })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .build()
}

/// HTTP `Date` header consistent with the reserve mocks' `expires_at_ms`
/// (1_700_000_000 s = Tue, 14 Nov 2023 22:13:20 GMT). Since heartbeat v2.3
/// the client derives no scheduling from `Date` (the first extend is
/// immediate) — the header is kept purely as realistic response furniture.
pub const HTTP_DATE: &str = "Tue, 14 Nov 2023 22:13:20 GMT";

/// Mount `POST /v1/reservations` responding `ALLOW` with the given id.
/// The heartbeat's first extend fires immediately (v2.3), so suites using
/// this must also mount the extend endpoint (`mount_extend`); the mocked
/// grant equals the default 60 s requested TTL, so after that first beat
/// the cadence settles at min(60000/2, 30 s) = 30 s — quiet for the
/// duration of any unrelated test.
pub async fn mount_reserve_allow(server: &MockServer, reservation_id: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("date", HTTP_DATE)
                .set_body_json(json!({
                    "decision": "ALLOW",
                    "reservation_id": reservation_id,
                    "affected_scopes": ["tenant:acme"],
                    "expires_at_ms": 1700000060000_u64
                })),
        )
        .mount(server)
        .await;
}

/// Mount the extend endpoint so heartbeat traffic succeeds.
pub async fn mount_extend(server: &MockServer, reservation_id: &str) {
    Mock::given(method("POST"))
        .and(path(format!("/v1/reservations/{reservation_id}/extend")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ACTIVE",
            "expires_at_ms": 1700000120000_u64
        })))
        .mount(server)
        .await;
}

/// Full guard-lifecycle scaffold: client plus mounted reserve
/// (`ALLOW_WITH_CAPS`, id `rsv_test`), extend, and release mocks.
pub async fn setup_with_reservation(server: &MockServer) -> CyclesClient {
    let client = CyclesClient::builder("key", server.uri())
        .journal_enabled(false)
        .build();

    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("date", HTTP_DATE)
                .set_body_json(json!({
                    "decision": "ALLOW_WITH_CAPS",
                    "reservation_id": "rsv_test",
                    "affected_scopes": ["tenant:acme", "app:my-app"],
                    "expires_at_ms": 1700000060000_u64,
                    "caps": {"max_tokens": 500, "max_steps_remaining": 10, "cooldown_ms": 1000}
                })),
        )
        .mount(server)
        .await;

    mount_extend(server, "rsv_test").await;

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
