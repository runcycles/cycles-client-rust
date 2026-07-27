//! Tests for the grant-ledger heartbeat (see `src/heartbeat.rs` module docs).
//!
//! v2.2: correctness rests on a conservative lead lower bound
//! `lead_min = grants_sum − elapsed` (grants are differences of successive
//! server-frame `expires_at_ms` values; no cross-clock arithmetic). A beat
//! skips only when `lead_min ≥ 1.5 · last_grant`; otherwise it extends by
//! the **requested** TTL. Beat delays are per-beat: the first is
//! `min(requested/2, 30 s, date_hint/2)` (the `Date`-derived TTL is a
//! cadence hint only), then `clamp(last_grant/2, 500 ms, requested/2)`.
//! These tests run a real short-TTL heartbeat against wiremock, with
//! responders that return `expires_at_ms` sequences the grant ledger reacts
//! to, and count extend calls per beat. The pure delay/lead computations are
//! unit-tested in `src/heartbeat.rs` (including the 30 s first-beat cap,
//! which would be impractical to wait out here).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use runcycles::models::*;
use runcycles::CyclesClient;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Unix ms of [`HTTP_DATE`] — the server-frame "now" stamped on the reserve
/// response. Paired with `expires_at_ms = DATE_MS + granted_ttl` so the
/// client derives exactly the intended first-beat cadence hint. (Without an
/// explicit pair, hyper's real Date header — years past this fixed expiry —
/// would saturate the raw hint to 0, which the client ignores.)
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
/// previous expiry, letting tests model full grants (`step = requested`)
/// and clamped grants (`step < requested`). The first response carries
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
/// (date hint == requested TTL, so the first beat lands at requested/2).
async fn reserve_with_ttl(
    server: &MockServer,
    rsv_id: &str,
    ttl_ms: u64,
) -> runcycles::ReservationGuard {
    mount_reserve(server, rsv_id, initial_expiry(ttl_ms), HTTP_DATE).await;
    do_reserve(server, ttl_ms).await
}

/// Bodies of the extend calls received so far, asserting each carried
/// `extend_by_ms == requested_ttl_ms` (v2.2 always extends by the request;
/// the server's clamp shows up in the grant ledger, not the wire amount).
async fn extend_bodies(
    server: &MockServer,
    rsv_id: &str,
    requested_ttl_ms: u64,
) -> Vec<serde_json::Value> {
    let requests = server.received_requests().await.unwrap();
    requests
        .iter()
        .filter(|r| r.url.path() == extend_path(rsv_id))
        .map(|r| {
            let body: serde_json::Value = r.body_json().unwrap();
            assert_eq!(
                body["extend_by_ms"], requested_ttl_ms,
                "extend amount must be the requested ttl"
            );
            body
        })
        .collect()
}

async fn extend_calls(server: &MockServer, rsv_id: &str, requested_ttl_ms: u64) -> usize {
    extend_bodies(server, rsv_id, requested_ttl_ms).await.len()
}

/// Full-grant steady state: with responses granting exactly the requested
/// TTL, `lead_min = grants_sum − elapsed` produces extend@1..4 (bounds
/// −1000, 0, 1000, 2000 against the 1.5·grant = 3000 threshold), skip@5
/// (bound exactly 3000, inclusive), extend@6 — the v2.2 cadence. Not
/// every-beat (drift) and not blind-alternate (which lapses under clamps).
#[tokio::test]
async fn heartbeat_lead_lower_bound_cadence() {
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
        "beat 1 must extend (no grant sample yet)"
    );

    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        2,
        "beat 2 must extend (lead_min 0)"
    );

    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        3,
        "beat 3 must extend (lead_min 1000 < 3000)"
    );

    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        4,
        "beat 4 must extend (lead_min 2000 < 3000)"
    );

    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        4,
        "beat 5 must skip (lead_min 3000 = 1.5*grant)"
    );

    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        5,
        "beat 6 must extend again (lead_min back to 2000)"
    );

    guard.release("test done").await.unwrap();
}

/// Tenant policy caps the grant below the request (governance
/// `max_reservation_ttl_ms`) and the create response has no effective-TTL
/// field. The `Date`-derived hint (2000) pulls the first beat to 1000 ms
/// (requested/2 would be 4000 ms — after expiry), and from then on the
/// cadence tracks the *observed* grants: responses grant +2000 per extend,
/// so beats stay at clamp(2000/2, 500, 4000) = 1000 ms — while the wire
/// `extend_by_ms` stays the REQUESTED 8000 (extend_bodies asserts this).
#[tokio::test]
async fn heartbeat_capped_grant_derives_cadence_from_observed_grants() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_capped";
    const REQUESTED: u64 = 8_000;
    const GRANTED: u64 = 2_000;
    const BEAT: Duration = Duration::from_millis(1_000);

    // The server grants only 2000 ms: expires_at = Date + 2000.
    mount_reserve(&server, rsv_id, initial_expiry(GRANTED), HTTP_DATE).await;
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(initial_expiry(GRANTED), GRANTED))
        .mount(&server)
        .await;

    let guard = do_reserve(&server, REQUESTED).await;

    // First beat at hint/2 = 1000 ms (requested/2 would be 4000 ms),
    // extending by the REQUESTED ttl (extend_bodies asserts the amount).
    tokio::time::sleep(BEAT + MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, REQUESTED).await,
        1,
        "beat 1 must fire at hint/2, not requested/2"
    );

    // Grants of 2000 keep the cadence at 1000 ms and the lead low:
    // beat 2 (lead_min 0) and beat 3 (lead_min 1000 < 3000) both extend.
    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, REQUESTED).await,
        2,
        "beat 2 at 2000 ms must extend (cadence follows the observed grant)"
    );
    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, REQUESTED).await,
        3,
        "beat 3 at 3000 ms must extend (lead_min 1000 < 1.5*grant)"
    );

    guard.release("test done").await.unwrap();
}

/// A garbage `Date` header means no hint: the first beat falls back to
/// `min(requested/2, 30 s)` — never to raw `expires_at − garbage`
/// arithmetic, and never to an immediate beat.
#[tokio::test]
async fn heartbeat_garbage_date_falls_back_to_requested_cadence() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_nodate";
    const TTL: u64 = 2_000;

    // Unparseable Date; the reported expiry (Date-frame + 500) is unusable
    // without the clock sample and must NOT shrink the first-beat delay.
    mount_reserve(&server, rsv_id, DATE_MS + 500, "not-a-date").await;
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(DATE_MS + 500, TTL))
        .mount(&server)
        .await;

    let guard = do_reserve(&server, TTL).await;

    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        0,
        "no hint: first-beat delay is requested/2 = 1000 ms, so no beat at 700 ms"
    );

    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "beat 1 at requested/2 extends by the requested ttl"
    );

    guard.release("test done").await.unwrap();
}

/// A transient failure (HTTP 500) is retried on the next beat — at the
/// current cadence, **with the same idempotency key** (a lost response must
/// not double-extend when the retry lands). After a success, the next
/// extend uses a fresh key.
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

    // Beat 1 attempts and fails (delay stays 1000 ms); beat 2 retries with
    // the same key and succeeds (grant 2000); beat 3 extends with a fresh
    // key (lead_min 2000 − 3000 < 0).
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
    assert_eq!(bodies.len(), 3, "beat 3 must extend (lead_min negative)");
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

/// Spec-legal small TTL (1000 < ttl < 2000): with hint == requested == 1200
/// the first beat lands at 600 ms and full grants keep the cadence at
/// clamp(1200/2, 500, 600) = 600 ms. Trace: extend@600/1200/1800/2400
/// (bounds −600, 0, 600, 1200 vs threshold 1800), skip@3000 (bound exactly
/// 1800). The reservation stays alive the whole way — a fixed 1-second
/// floor would have guaranteed a lapse here.
#[tokio::test]
async fn heartbeat_small_ttl_stays_alive() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_small";
    const TTL: u64 = 1_200;
    // Beat length is 600 ms; the sleeps below are cumulative absolute
    // offsets (850 ms, 2700 ms, 3350 ms) chosen to land mid-gap.

    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(initial_expiry(TTL), TTL))
        .mount(&server)
        .await;

    let guard = reserve_with_ttl(&server, rsv_id, TTL).await;

    // 850 ms in: the 600 ms first beat has fired. Under a 1-second floor no
    // request would exist yet — and the reservation would already be past
    // half-life with nothing scheduled before expiry.
    tokio::time::sleep(Duration::from_millis(850)).await;
    assert!(
        extend_calls(&server, rsv_id, TTL).await >= 1,
        "first beat must land at ttl/2 = 600 ms, no 1-second floor"
    );

    // Through beat 4 (2400 ms) + margin: all four beats extend.
    tokio::time::sleep(Duration::from_millis(1_850)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        4,
        "beats at 600/1200/1800/2400 ms all extend (lead_min below 1.5*grant)"
    );

    // Beat 5 (3000 ms): lead_min = 4800 − 3000 = 1800 = 1.5·grant → skip.
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        4,
        "beat 5 skips once the lead lower bound reaches 1.5*grant"
    );

    guard.release("test done").await.unwrap();
}

/// A server that clamps grants (extends by only requested/4 per call) keeps
/// `lead_min` pinned below the skip threshold, so every beat extends — and
/// the cadence *speeds up* to track the observed grant:
/// clamp(500/2, 500, 1000) = 500 ms beats after the first. The fixed
/// alternate-beat cadence would have lapsed the reservation instead.
#[tokio::test]
async fn heartbeat_clamped_grant_extends_every_beat() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_clamp";
    const TTL: u64 = 2_000;

    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(initial_expiry(TTL), TTL / 4))
        .mount(&server)
        .await;

    let guard = reserve_with_ttl(&server, rsv_id, TTL).await;

    // Beat 1 at 1000 ms (grant 500 → 500 ms cadence), then beats at 1500
    // and 2000 ms: 3 extends by t=2250. lead_min stays at −1000 (each 500
    // grant is consumed by the 500 ms gap), so no beat ever skips.
    tokio::time::sleep(Duration::from_millis(2_250)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        3,
        "clamped grants: beats at 1000/1500/2000 ms all extend"
    );

    // Beats at 2500 and 3000 ms: still extending every beat.
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        5,
        "clamped grants keep the lead low: every beat must extend"
    );

    guard.release("test done").await.unwrap();
}

/// An HTTP 200 whose status is unrecognized (forward-compat `Unknown`) still
/// counts as **applied**: a 2xx means the server DID extend, and its
/// `expires_at_ms` is authoritative. The heartbeat feeds it into the grant
/// ledger (warning only) — pinned by the fresh key on beat 2 (a failure
/// would have kept the key) and by beat 5 skipping exactly as in the
/// all-ACTIVE cadence (a failure would have left the grant uncounted and
/// beat 5 extending).
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

    tokio::time::sleep(BEAT).await;
    let bodies = extend_bodies(&server, rsv_id, TTL).await;
    assert_eq!(bodies.len(), 2, "beat 2 extends (lead_min 0)");
    assert_ne!(
        bodies[0]["idempotency_key"], bodies[1]["idempotency_key"],
        "the unknown-status 200 resolved the attempt: beat 2 must use a \
         fresh key, not retry the old one"
    );

    // Beats 3 and 4 extend; beat 5 skips (lead_min 8000 − 5000 = 3000 =
    // 1.5·grant) — proof the SUSPENDED response's grant entered the ledger.
    tokio::time::sleep(BEAT + BEAT + BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        4,
        "a 2xx with unknown status must count as applied: beat 5 skips"
    );

    guard.release("test done").await.unwrap();
}
