//! Tests for the lead-estimate heartbeat (see `src/heartbeat.rs` module docs).
//!
//! The heartbeat ticks every `ttl/2` (no floor) and extends only when its
//! estimated expiry lead — `(known_expiry - initial_expiry) + ttl - elapsed`,
//! server-frame minus server-frame, monotonic minus monotonic — has fallen
//! below `1.5·ttl`, where `ttl` is the *effective* TTL recovered from the
//! reserve response's `expires_at_ms` and HTTP `Date` header. These tests run
//! a real short-TTL heartbeat against wiremock, with responders that return
//! `expires_at_ms` sequences the lead math reacts to, and count extend calls
//! per beat.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use runcycles::models::*;
use runcycles::CyclesClient;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Unix ms of [`HTTP_DATE`] — the server-frame "now" stamped on the reserve
/// response. Paired with `expires_at_ms = DATE_MS + granted_ttl` so the
/// client derives exactly the intended effective TTL. (Without an explicit
/// pair, hyper's real Date header — years past this fixed expiry — would
/// clamp every test's effective TTL to the 1000 ms minimum.)
const DATE_MS: u64 = 1_700_000_000_000;
const HTTP_DATE: &str = "Tue, 14 Nov 2023 22:13:20 GMT";
/// Margin past a beat: long enough to absorb scheduler and loopback-HTTP
/// latency, short of the following beat.
const MARGIN: Duration = Duration::from_millis(450);

fn extend_path(rsv_id: &str) -> String {
    format!("/v1/reservations/{rsv_id}/extend")
}

/// The initial expiry a reserve granting `ttl` reports: `DATE_MS + ttl`.
fn initial_expiry(granted_ttl_ms: u64) -> u64 {
    DATE_MS + granted_ttl_ms
}

/// Responds to extend calls with `expires_at_ms = base + n·step` for the
/// n-th call — i.e. every response grants exactly `step` on top of the
/// previous expiry, letting tests model full grants (`step = ttl`) and
/// clamped grants (`step = ttl/4`). The first response carries
/// `first_status`, the rest `rest_status`.
struct ExpirySequence {
    base: u64,
    step: u64,
    first_status: &'static str,
    rest_status: &'static str,
    calls: AtomicU64,
}

impl ExpirySequence {
    fn granting(base: u64, step: u64) -> Self {
        Self {
            base,
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
            "expires_at_ms": self.base + n * self.step
        }))
    }
}

/// Mount the reserve + release mocks. The reserve response reports
/// `expires_at_ms` and carries `date_header` as its HTTP `Date`.
async fn mount_reserve(server: &MockServer, rsv_id: &str, expires_at_ms: u64, date_header: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("date", date_header)
                .set_body_json(json!({
                    "decision": "ALLOW",
                    "reservation_id": rsv_id,
                    "affected_scopes": ["tenant:acme"],
                    "expires_at_ms": expires_at_ms
                })),
        )
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
}

/// Reserve with the given requested TTL against already-mounted mocks.
async fn do_reserve(server: &MockServer, requested_ttl_ms: u64) -> runcycles::ReservationGuard {
    let client = CyclesClient::builder("key", server.uri()).build();
    let req = ReservationCreateRequest::builder()
        .subject(Subject {
            tenant: Some("acme".into()),
            ..Default::default()
        })
        .action(Action::new("llm.completion", "gpt-4o"))
        .estimate(Amount::usd_microcents(5000))
        .ttl_ms(requested_ttl_ms)
        .build();
    client.reserve(req).await.unwrap()
}

/// Reserve where the server grants exactly what was requested
/// (effective TTL == requested TTL).
async fn reserve_with_ttl(
    server: &MockServer,
    rsv_id: &str,
    ttl_ms: u64,
) -> runcycles::ReservationGuard {
    mount_reserve(server, rsv_id, initial_expiry(ttl_ms), HTTP_DATE).await;
    do_reserve(server, ttl_ms).await
}

/// Bodies of the extend calls received so far, asserting each carried
/// `extend_by_ms == effective_ttl_ms`.
async fn extend_bodies(
    server: &MockServer,
    rsv_id: &str,
    effective_ttl_ms: u64,
) -> Vec<serde_json::Value> {
    let requests = server.received_requests().await.unwrap();
    requests
        .iter()
        .filter(|r| r.url.path() == extend_path(rsv_id))
        .map(|r| {
            let body: serde_json::Value = r.body_json().unwrap();
            assert_eq!(
                body["extend_by_ms"], effective_ttl_ms,
                "extend amount must be the effective ttl"
            );
            body
        })
        .collect()
}

async fn extend_calls(server: &MockServer, rsv_id: &str, effective_ttl_ms: u64) -> usize {
    extend_bodies(server, rsv_id, effective_ttl_ms).await.len()
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
        .respond_with(ExpirySequence::granting(initial_expiry(TTL), TTL))
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

/// Tenant policy caps the granted TTL below the request (governance
/// `max_reservation_ttl_ms`), and the create response has no effective-TTL
/// field — the client must recover the grant from `expires_at_ms - Date` and
/// derive EVERYTHING from it: beat interval, lead math, and extend amount.
/// Requested 8000 / granted 2000: seeding from the request would put the
/// first beat at 4000 ms — 2000 ms after expiry.
#[tokio::test]
async fn heartbeat_capped_ttl_derives_cadence_from_effective() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_capped";
    const REQUESTED: u64 = 8_000;
    const EFFECTIVE: u64 = 2_000;
    const BEAT: Duration = Duration::from_millis(1_000); // EFFECTIVE / 2

    // The server grants only 2000 ms: expires_at = Date + 2000.
    mount_reserve(&server, rsv_id, initial_expiry(EFFECTIVE), HTTP_DATE).await;
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(
            initial_expiry(EFFECTIVE),
            EFFECTIVE,
        ))
        .mount(&server)
        .await;

    let guard = do_reserve(&server, REQUESTED).await;

    // First beat at effective/2 = 1000 ms (requested/2 would be 4000 ms),
    // extending by the EFFECTIVE ttl (extend_bodies asserts the amount).
    tokio::time::sleep(BEAT + MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, EFFECTIVE).await,
        1,
        "beat 1 must fire at effective/2, not requested/2"
    );

    // And the full lead cadence runs on the effective ttl: extend@2, skip@3.
    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, EFFECTIVE).await,
        2,
        "beat 2 must extend (lead ttl)"
    );
    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, EFFECTIVE).await,
        2,
        "beat 3 must skip (lead 1.5*effective)"
    );

    guard.release("test done").await.unwrap();
}

/// A garbage `Date` header means no server clock sample: the effective TTL
/// falls back to the requested one (never to the raw `expires_at - garbage`
/// arithmetic, and never to zero).
#[tokio::test]
async fn heartbeat_garbage_date_falls_back_to_requested_ttl() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_nodate";
    const TTL: u64 = 2_000;

    // Unparseable Date; the reported expiry (Date-frame + 500) is unusable
    // without the clock sample and must NOT shrink the TTL.
    mount_reserve(&server, rsv_id, DATE_MS + 500, "not-a-date").await;
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(DATE_MS + 500, TTL))
        .mount(&server)
        .await;

    let guard = do_reserve(&server, TTL).await;

    // Had the pair been (mis)used, effective would clamp to 1000 ms and the
    // first beat would land at 500 ms.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        0,
        "fallback interval is requested/2 = 1000 ms, so no beat at 700 ms"
    );

    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "beat 1 at requested/2 extends by the requested ttl"
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
        .respond_with(ExpirySequence::granting(initial_expiry(TTL), TTL))
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

/// Scaffold for the permanent-failure tests: every extend gets `status` +
/// `error_code`; the heartbeat must attempt exactly once and then stop —
/// no further requests on later beats.
async fn assert_permanent_stop(rsv_id: &str, status: u16, error_code: &str) {
    let server = MockServer::start().await;
    const TTL: u64 = 2_000;
    const BEAT: Duration = Duration::from_millis(1_000);

    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({
            "error": error_code,
            "message": "permanent"
        })))
        .mount(&server)
        .await;

    let guard = reserve_with_ttl(&server, rsv_id, TTL).await;

    tokio::time::sleep(BEAT + MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "beat 1 attempts and hits the permanent failure ({error_code})"
    );

    tokio::time::sleep(BEAT + BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "the heartbeat must stop after {error_code} — no retries"
    );

    guard.release("test done").await.unwrap();
}

/// 409 `MAX_EXTENSIONS_EXCEEDED`: the extension allowance is exhausted for
/// good — the heartbeat terminates.
#[tokio::test]
async fn heartbeat_stops_on_max_extensions_exceeded() {
    assert_permanent_stop("rsv_hb_perm", 409, "MAX_EXTENSIONS_EXCEEDED").await;
}

/// 409 `TENANT_CLOSED`: tenant closure is irreversible without administrative
/// action — the heartbeat terminates.
#[tokio::test]
async fn heartbeat_stops_on_tenant_closed() {
    assert_permanent_stop("rsv_hb_closed", 409, "TENANT_CLOSED").await;
}

/// 404 `NOT_FOUND`: a 404'd reservation never comes back — the heartbeat
/// terminates.
#[tokio::test]
async fn heartbeat_stops_on_not_found() {
    assert_permanent_stop("rsv_hb_404", 404, "NOT_FOUND").await;
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
        .respond_with(ExpirySequence::granting(initial_expiry(TTL), TTL))
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
        .respond_with(ExpirySequence::granting(initial_expiry(TTL), TTL / 4))
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
            base: initial_expiry(TTL),
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
