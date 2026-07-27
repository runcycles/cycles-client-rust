//! Tests for the lead-estimate heartbeat (see `src/heartbeat.rs` module docs).
//!
//! The heartbeat ticks every `ttl/2` (no floor) and extends only when its
//! estimated expiry lead — `(known_expiry - initial_expiry) + ttl - elapsed`,
//! server-frame minus server-frame, monotonic minus monotonic — has fallen
//! below `1.5·ttl`. These tests run a real short-TTL heartbeat against
//! wiremock, with responders that return `expires_at_ms` sequences the lead
//! math reacts to, and count extend calls per beat.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use runcycles::models::*;
use runcycles::CyclesClient;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Server-frame expiry stamped on the reserve response.
const INITIAL_EXPIRY: u64 = 1_700_000_001_000;
/// Margin past a beat: long enough to absorb scheduler and loopback-HTTP
/// latency, short of the following beat.
const MARGIN: Duration = Duration::from_millis(450);

fn extend_path(rsv_id: &str) -> String {
    format!("/v1/reservations/{rsv_id}/extend")
}

/// Responds to extend calls with `expires_at_ms = INITIAL_EXPIRY + n·step`
/// for the n-th call — i.e. every response grants exactly `step` on top of
/// the previous expiry, letting tests model full grants (`step = ttl`) and
/// clamped grants (`step = ttl/4`). The first response carries
/// `first_status`, the rest `rest_status`.
struct ExpirySequence {
    step: u64,
    first_status: &'static str,
    rest_status: &'static str,
    calls: AtomicU64,
}

impl ExpirySequence {
    fn granting(step: u64) -> Self {
        Self {
            step,
            first_status: "ACTIVE",
            rest_status: "ACTIVE",
            calls: AtomicU64::new(0),
        }
    }
}

impl Respond for ExpirySequence {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let status = if n == 1 {
            self.first_status
        } else {
            self.rest_status
        };
        ResponseTemplate::new(200).set_body_json(json!({
            "status": status,
            "expires_at_ms": INITIAL_EXPIRY + n * self.step
        }))
    }
}

async fn reserve_with_ttl(
    server: &MockServer,
    rsv_id: &str,
    ttl_ms: u64,
) -> runcycles::ReservationGuard {
    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "ALLOW",
            "reservation_id": rsv_id,
            "affected_scopes": ["tenant:acme"],
            "expires_at_ms": INITIAL_EXPIRY
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
        .ttl_ms(ttl_ms)
        .build();
    client.reserve(req).await.unwrap()
}

/// Bodies of the extend calls received so far, asserting each carried
/// `extend_by_ms == ttl_ms`.
async fn extend_bodies(server: &MockServer, rsv_id: &str, ttl_ms: u64) -> Vec<serde_json::Value> {
    let requests = server.received_requests().await.unwrap();
    requests
        .iter()
        .filter(|r| r.url.path() == extend_path(rsv_id))
        .map(|r| {
            let body: serde_json::Value = r.body_json().unwrap();
            assert_eq!(
                body["extend_by_ms"], ttl_ms,
                "extend amount must stay ttl_ms"
            );
            body
        })
        .collect()
}

async fn extend_calls(server: &MockServer, rsv_id: &str, ttl_ms: u64) -> usize {
    extend_bodies(server, rsv_id, ttl_ms).await.len()
}

/// Full-grant steady state: with responses returning `prev + ttl`, the lead
/// estimate produces extend@1, extend@2 (leads ttl/2 and ttl), skip@3 (lead
/// exactly 1.5·ttl), extend@4 — the v2 cadence. Not every-beat (drift) and
/// not blind-alternate (which would skip beat 2 and run a lower lead).
#[tokio::test]
async fn heartbeat_lead_estimate_cadence() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_lead";
    const TTL: u64 = 2_000;
    const BEAT: Duration = Duration::from_millis(1_000);

    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(TTL))
        .mount(&server)
        .await;

    let guard = reserve_with_ttl(&server, rsv_id, TTL).await;

    tokio::time::sleep(BEAT + MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "beat 1 must extend (lead ttl/2)"
    );

    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        2,
        "beat 2 must extend (lead ttl < 1.5*ttl)"
    );

    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        2,
        "beat 3 must skip (lead 1.5*ttl)"
    );

    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        3,
        "beat 4 must extend again (lead back to ttl)"
    );

    guard.release("test done").await.unwrap();
}

/// A transient failure (HTTP 500) is retried on the next beat **with the same
/// idempotency key** — a lost response must not double-extend when the retry
/// lands. After a success, the next extend uses a fresh key.
#[tokio::test]
async fn heartbeat_retry_reuses_idempotency_key_until_success() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_key";
    const TTL: u64 = 2_000;
    const BEAT: Duration = Duration::from_millis(1_000);

    // First extend attempt fails...
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "INTERNAL_ERROR",
            "message": "boom"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // ...subsequent attempts succeed with full grants.
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(TTL))
        .mount(&server)
        .await;

    let guard = reserve_with_ttl(&server, rsv_id, TTL).await;

    // Beat 1 attempts and fails; beat 2 retries (lead has kept shrinking);
    // beat 3 extends with the post-success lead of ttl.
    tokio::time::sleep(BEAT + MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "beat 1 attempts"
    );

    tokio::time::sleep(BEAT).await;
    let bodies = extend_bodies(&server, rsv_id, TTL).await;
    assert_eq!(bodies.len(), 2, "beat 2 must retry the failed extend");
    assert_eq!(
        bodies[0]["idempotency_key"], bodies[1]["idempotency_key"],
        "the retry must reuse the failed attempt's idempotency key \
         (dedupe against an applied-but-lost extension)"
    );

    tokio::time::sleep(BEAT).await;
    let bodies = extend_bodies(&server, rsv_id, TTL).await;
    assert_eq!(bodies.len(), 3, "beat 3 must extend (lead ttl)");
    assert_ne!(
        bodies[2]["idempotency_key"], bodies[0]["idempotency_key"],
        "after a success the next extend must use a fresh idempotency key"
    );

    guard.release("test done").await.unwrap();
}

/// A permanent extend failure (409 `MAX_EXTENSIONS_EXCEEDED`) terminates the
/// heartbeat: no further extend can ever succeed, so no further requests may
/// be sent.
#[tokio::test]
async fn heartbeat_stops_on_permanent_failure() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_perm";
    const TTL: u64 = 2_000;
    const BEAT: Duration = Duration::from_millis(1_000);

    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "MAX_EXTENSIONS_EXCEEDED",
            "message": "extension allowance exhausted"
        })))
        .mount(&server)
        .await;

    let guard = reserve_with_ttl(&server, rsv_id, TTL).await;

    tokio::time::sleep(BEAT + MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "beat 1 attempts and hits the permanent failure"
    );

    tokio::time::sleep(BEAT + BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "the heartbeat must stop after a permanent failure — no retries"
    );

    guard.release("test done").await.unwrap();
}

/// Spec-legal small TTL (1000 < ttl < 2000): the removed 1-second interval
/// floor would have guaranteed a lapse (first beat at 1000 ms with only
/// 1200 ms of lifetime, then a full skip cycle). With interval = ttl/2 the
/// first extend lands at 600 ms and the reservation stays alive across four
/// beats.
#[tokio::test]
async fn heartbeat_small_ttl_stays_alive() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_small";
    const TTL: u64 = 1_200;
    // Beat length is ttl/2 = 600 ms; the sleeps below are cumulative
    // absolute offsets (850 ms, then 2700 ms) chosen to land mid-gap.

    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(TTL))
        .mount(&server)
        .await;

    let guard = reserve_with_ttl(&server, rsv_id, TTL).await;

    // 850 ms in: the ttl/2 = 600 ms beat has fired. Under the old 1-second
    // floor no request would exist yet — and the reservation would already
    // be 150 ms past half-life with nothing scheduled before expiry.
    tokio::time::sleep(Duration::from_millis(850)).await;
    assert!(
        extend_calls(&server, rsv_id, TTL).await >= 1,
        "interval must be ttl/2 with no 1-second floor"
    );

    // Through beat 4 (2400 ms) + margin: extend@1, extend@2, skip@3,
    // extend@4 — alive the whole way (minimum lead estimate ttl/2).
    tokio::time::sleep(Duration::from_millis(1_850)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        3,
        "four beats: extend, extend, skip, extend"
    );

    guard.release("test done").await.unwrap();
}

/// A server that clamps grants (extends by only ttl/4 per call) keeps the
/// lead estimate below the skip threshold, so every beat extends — the fixed
/// alternate-beat cadence would have lapsed the reservation instead.
#[tokio::test]
async fn heartbeat_clamped_grant_extends_every_beat() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_clamp";
    const TTL: u64 = 2_000;
    const BEAT: Duration = Duration::from_millis(1_000);

    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(TTL / 4))
        .mount(&server)
        .await;

    let guard = reserve_with_ttl(&server, rsv_id, TTL).await;

    tokio::time::sleep(BEAT * 4 + MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        4,
        "clamped grants keep the lead low: every beat must extend"
    );

    guard.release("test done").await.unwrap();
}

/// An HTTP 200 whose status is unrecognized (forward-compat `Unknown`) still
/// counts as **applied**: a 2xx means the server DID extend, and its
/// `expires_at_ms` is authoritative. The heartbeat updates its lead from the
/// response (warning only) — pinned by beat 3 skipping exactly as in the
/// all-ACTIVE cadence. This reverses the previous non-ACTIVE-as-failure
/// behavior, which would have re-extended every beat (drift) against any
/// server sending a newer status string.
#[tokio::test]
async fn heartbeat_2xx_unknown_status_counts_as_applied() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_unknown";
    const TTL: u64 = 2_000;
    const BEAT: Duration = Duration::from_millis(1_000);

    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence {
            step: TTL,
            first_status: "SUSPENDED",
            rest_status: "ACTIVE",
            calls: AtomicU64::new(0),
        })
        .mount(&server)
        .await;

    let guard = reserve_with_ttl(&server, rsv_id, TTL).await;

    tokio::time::sleep(BEAT + MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "beat 1 extends and receives the unknown-status 200"
    );

    // Beats 2 and 3: if the unknown status had been treated as failure the
    // lead would never grow and beat 3 would extend too (3 calls). Success
    // treatment yields extend@2 then skip@3 (2 calls).
    tokio::time::sleep(BEAT + BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        2,
        "a 2xx with unknown status must count as applied: beat 3 skips"
    );

    guard.release("test done").await.unwrap();
}
