//! Tests for the reservation heartbeat (see `src/heartbeat.rs` module docs).
//!
//! Round 5 (spec PR #148): when a create/extend response carries
//! `remaining_ttl_ms`, scheduling is **normative** — the next beat lands
//! `lead_floor − retry_reserve` after response receipt, the skip heuristic
//! is bypassed, and no primed first extension is spent. The tests in the
//! second half of this file pin that mode, including the mid-flight
//! fallback when the field disappears.
//!
//! Fallback (no field), v2.3: the first extend fires **immediately** (no bounded delay can outlive
//! an arbitrarily small tenant-capped lease, so none is used) and primes the
//! grant ledger. Correctness rests on a conservative lead lower bound
//! `lead_min = grants_sum − elapsed` (grants are differences of successive
//! server-frame `expires_at_ms` values; no cross-clock arithmetic). A beat
//! skips only when `lead_min ≥ 1.5 · last_grant`; otherwise it extends by
//! the **requested** TTL. After a success the cadence is
//! `clamp(grant/2, 500 ms, requested/2)` — unless the grant looks like a
//! maximum-LEAD clamp (grant ≈ elapsed instead of ≈ requested), in which
//! case the cadence is held at `min(requested/2, 30 s)` so the observed
//! "grants" (which merely measure elapsed time) cannot collapse the cadence
//! to the floor and burn the extension allowance.
//!
//! These tests run a real short-TTL heartbeat against wiremock, with
//! responders that return `expires_at_ms` sequences the grant ledger reacts
//! to, and count extend calls per beat. The pure cadence/regime computations
//! are unit-tested in `src/heartbeat.rs` (including the 30 s held-cadence
//! cap, which would be impractical to wait out here).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use runcycles::models::*;
use runcycles::CyclesClient;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Arbitrary server-frame epoch for the mocks' `expires_at_ms` values. Only
/// *differences* between successive values matter to the client (heartbeat
/// v2.3 does no cross-clock arithmetic at all).
const BASE_MS: u64 = 1_700_000_000_000;
/// Margin past a beat: long enough to absorb scheduler and loopback-HTTP
/// latency, short of the following beat.
const MARGIN: Duration = Duration::from_millis(450);

fn extend_path(rsv_id: &str) -> String {
    format!("/v1/reservations/{rsv_id}/extend")
}

/// The initial expiry a reserve granting `ttl` reports: `BASE_MS + ttl`.
fn initial_expiry(granted_ttl_ms: u64) -> u64 {
    BASE_MS + granted_ttl_ms
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
    /// When set, every response also carries `remaining_ttl_ms` with this
    /// constant value (spec PR #148 — models a server that always reports
    /// the same remaining lease).
    remaining_ttl_ms: Option<u64>,
    /// When set with `remaining_ttl_ms`, only the first `n` responses carry
    /// the field — models a mid-flight rollback to a server without it.
    remaining_on_first_n: Option<u64>,
    calls: AtomicU64,
}

impl ExpirySequence {
    fn granting(base: u64, step: u64) -> Self {
        Self {
            base,
            step,
            first_status: "ACTIVE",
            rest_status: "ACTIVE",
            remaining_ttl_ms: None,
            remaining_on_first_n: None,
            calls: AtomicU64::new(0),
        }
    }

    fn with_remaining(mut self, remaining_ttl_ms: u64) -> Self {
        self.remaining_ttl_ms = Some(remaining_ttl_ms);
        self
    }

    fn remaining_only_on_first(mut self, n: u64) -> Self {
        self.remaining_on_first_n = Some(n);
        self
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
        let mut body = json!({
            "status": status,
            "expires_at_ms": self.base + n * self.step
        });
        if let Some(remaining) = self.remaining_ttl_ms {
            if self.remaining_on_first_n.is_none_or(|first_n| n <= first_n) {
                body["remaining_ttl_ms"] = json!(remaining);
            }
        }
        ResponseTemplate::new(200).set_body_json(body)
    }
}

/// Models a maximum-LEAD clamp: every extend re-stamps
/// `expires_at ≈ "now" + L` instead of adding lease, so successive
/// `expires_at_ms` differences measure *elapsed time between requests*, not
/// granted lease. Implemented as `base + elapsed-since-creation` at the
/// received-at instant (the constant L cancels out of every difference and
/// is therefore irrelevant to the client's ledger).
struct LeadClampEcho {
    base: u64,
    created: std::time::Instant,
    /// When set, responses also carry `remaining_ttl_ms` with this constant
    /// value — a spec-PR-#148 server whose lead clamp holds the remaining
    /// lease at the cap.
    remaining_ttl_ms: Option<u64>,
}

impl LeadClampEcho {
    fn holding(base: u64) -> Self {
        Self {
            base,
            created: std::time::Instant::now(),
            remaining_ttl_ms: None,
        }
    }

    fn with_remaining(mut self, remaining_ttl_ms: u64) -> Self {
        self.remaining_ttl_ms = Some(remaining_ttl_ms);
        self
    }
}

impl Respond for LeadClampEcho {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let elapsed = u64::try_from(self.created.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut body = json!({
            "status": "ACTIVE",
            "expires_at_ms": self.base + elapsed
        });
        if let Some(remaining) = self.remaining_ttl_ms {
            body["remaining_ttl_ms"] = json!(remaining);
        }
        ResponseTemplate::new(200).set_body_json(body)
    }
}

/// Mount the reserve + release mocks. The reserve response reports
/// `expires_at_ms` and, when `remaining_ttl_ms` is `Some`, the spec-PR-#148
/// remaining-lease field (normative heartbeat scheduling).
async fn mount_reserve_full(
    server: &MockServer,
    rsv_id: &str,
    expires_at_ms: u64,
    remaining_ttl_ms: Option<u64>,
) {
    let mut body = json!({
        "decision": "ALLOW",
        "reservation_id": rsv_id,
        "affected_scopes": ["tenant:acme"],
        "expires_at_ms": expires_at_ms
    });
    if let Some(remaining) = remaining_ttl_ms {
        body["remaining_ttl_ms"] = json!(remaining);
    }
    Mock::given(method("POST"))
        .and(path("/v1/reservations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
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

/// Legacy-server reserve mock: no `remaining_ttl_ms` (fallback heartbeat).
async fn mount_reserve(server: &MockServer, rsv_id: &str, expires_at_ms: u64) {
    mount_reserve_full(server, rsv_id, expires_at_ms, None).await;
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

/// Reserve where the server grants exactly what was requested.
async fn reserve_with_ttl(
    server: &MockServer,
    rsv_id: &str,
    ttl_ms: u64,
) -> runcycles::ReservationGuard {
    mount_reserve(server, rsv_id, initial_expiry(ttl_ms)).await;
    do_reserve(server, ttl_ms).await
}

/// Bodies of the extend calls received so far, asserting each carried
/// `extend_by_ms == requested_ttl_ms` (the heartbeat always extends by the
/// request; the server's clamp shows up in the grant ledger, not the wire
/// amount).
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

/// Immediate first beat + full-grant steady state. The first extend fires
/// at once (well before ttl/2 — asserted inside the first margin) and
/// primes the ledger; with responses granting exactly the requested TTL,
/// `lead_min = grants_sum − elapsed` then produces extend@2..3 (bounds
/// 1000, 2000 against the 1.5·grant = 3000 threshold), skip@4 (bound
/// exactly 3000, inclusive), extend@5 — the v2.3 cadence. Not every-beat
/// (drift) and not blind-alternate (which lapses under clamps).
#[tokio::test]
async fn heartbeat_first_beat_immediate_then_lead_bound_cadence() {
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

    tokio::time::sleep(MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "the first extend must fire immediately — well before ttl/2 = 1000 ms \
         (any bounded delay can outlive a small tenant-capped lease)"
    );

    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        2,
        "beat 2 at 1000 ms must extend (lead_min 1000 < 3000)"
    );

    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        3,
        "beat 3 at 2000 ms must extend (lead_min 2000 < 3000)"
    );

    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        3,
        "beat 4 at 3000 ms must skip (lead_min 3000 = 1.5·grant, inclusive)"
    );

    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        4,
        "beat 5 at 4000 ms must extend again (lead_min back to 2000)"
    );

    guard.release("test done").await.unwrap();
}

/// Tenant policy caps the lease below the request (governance
/// `max_reservation_ttl_ms`) and the create response has no effective-TTL
/// field. The immediate first beat discovers the real grant at once — a
/// `requested/2` first beat would fire at 4000 ms, 2000 ms after the capped
/// lease expired — and from then on the cadence tracks the *observed*
/// per-extend grants (+2000 each → 1000 ms beats), while the wire
/// `extend_by_ms` stays the REQUESTED 8000 (`extend_bodies` asserts this).
/// The real per-extend grant is ≈ 2× the inter-success elapsed, outside the
/// lead-clamp band, so the cadence is NOT held at requested/2.
#[tokio::test]
async fn heartbeat_capped_grant_derives_cadence_from_observed_grants() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_capped";
    const REQUESTED: u64 = 8_000;
    const GRANTED: u64 = 2_000;
    const BEAT: Duration = Duration::from_millis(1_000);

    // The server grants only 2000 ms of lease at reserve and per extend.
    mount_reserve(&server, rsv_id, initial_expiry(GRANTED)).await;
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(initial_expiry(GRANTED), GRANTED))
        .mount(&server)
        .await;

    let guard = do_reserve(&server, REQUESTED).await;

    tokio::time::sleep(MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, REQUESTED).await,
        1,
        "the first beat must be immediate — the only schedule that cannot \
         outlive the silently-capped 2000 ms lease"
    );

    // Grants of 2000 set the cadence to clamp(2000/2, 500, 4000) = 1000 ms
    // and keep the lead low: beat 2 (lead_min 1000) and beat 3 (lead_min
    // 2000, both < 3000) extend.
    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, REQUESTED).await,
        2,
        "beat 2 at 1000 ms must extend (cadence follows the observed grant)"
    );
    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, REQUESTED).await,
        3,
        "beat 3 at 2000 ms must extend (lead_min 2000 < 1.5·grant)"
    );

    guard.release("test done").await.unwrap();
}

/// A transient failure (HTTP 503) on the immediate first beat is retried at
/// the **held cadence** `min(requested/2, 30 s)` — never immediately again
/// (no hot loop against a down server) — **with the same idempotency key**
/// (a lost response must not double-extend when the retry lands). After a
/// success, the next extend uses a fresh key.
#[tokio::test]
async fn heartbeat_first_beat_failure_retries_at_held_cadence_with_same_key() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_key";
    const TTL: u64 = 2_000;
    const BEAT: Duration = Duration::from_millis(1_000);

    // The immediate first attempt fails...
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
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

    // Beat 1 fires immediately and fails; the retry is scheduled at the
    // held cadence min(2000/2, 30 s) = 1000 ms — exactly ONE attempt must
    // exist inside the first margin (a zero-delay retry loop would show
    // dozens).
    tokio::time::sleep(MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "the failed immediate beat must wait the held cadence before \
         retrying — never retry at zero delay"
    );

    tokio::time::sleep(BEAT).await;
    let bodies = extend_bodies(&server, rsv_id, TTL).await;
    assert_eq!(
        bodies.len(),
        2,
        "beat 2 at 1000 ms must retry the failed extend"
    );
    assert_eq!(
        bodies[0]["idempotency_key"], bodies[1]["idempotency_key"],
        "the retry must reuse the failed attempt's idempotency key \
         (dedupe against an applied-but-lost extension)"
    );

    tokio::time::sleep(BEAT).await;
    let bodies = extend_bodies(&server, rsv_id, TTL).await;
    assert_eq!(bodies.len(), 3, "beat 3 must extend (lead_min 0 < 3000)");
    assert_ne!(
        bodies[2]["idempotency_key"], bodies[0]["idempotency_key"],
        "after a success the next extend must use a fresh idempotency key"
    );

    guard.release("test done").await.unwrap();
}

/// Scaffold for the permanent-failure tests: every extend gets `status` +
/// `error_code`; the heartbeat must attempt exactly once (immediately) and
/// then stop — no further requests on later beats.
async fn assert_permanent_stop(rsv_id: &str, status: u16, error_code: &str) {
    let server = MockServer::start().await;
    const TTL: u64 = 2_000;

    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({
            "error": error_code,
            "message": "permanent"
        })))
        .mount(&server)
        .await;

    let guard = reserve_with_ttl(&server, rsv_id, TTL).await;

    tokio::time::sleep(MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "the immediate beat attempts and hits the permanent failure ({error_code})"
    );

    // Two full held-cadence periods later: still exactly one attempt.
    tokio::time::sleep(Duration::from_millis(2_000)).await;
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

/// Spec-legal small TTL (1000 < ttl < 2000): the first beat is immediate
/// and full grants of 1200 set the cadence to clamp(600, 500, 600) =
/// 600 ms. Trace: extend@0/600/1200 (bounds 0, 600, 1200 vs threshold
/// 1800), skip@1800 (bound exactly 1800), extend@2400. The reservation
/// stays alive the whole way — a fixed 1-second floor would have
/// guaranteed a lapse here.
#[tokio::test]
async fn heartbeat_small_ttl_stays_alive() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_small";
    const TTL: u64 = 1_200;
    // Beat length is 600 ms; the sleeps below land 250 ms past each beat.
    const SMALL_MARGIN: Duration = Duration::from_millis(250);
    const SMALL_BEAT: Duration = Duration::from_millis(600);

    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(initial_expiry(TTL), TTL))
        .mount(&server)
        .await;

    let guard = reserve_with_ttl(&server, rsv_id, TTL).await;

    tokio::time::sleep(SMALL_MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "the first beat must be immediate"
    );

    tokio::time::sleep(SMALL_BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        2,
        "beat 2 at 600 ms must extend (no 1-second floor; lead_min 600 < 1800)"
    );

    tokio::time::sleep(SMALL_BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        3,
        "beat 3 at 1200 ms must extend (lead_min 1200 < 1800)"
    );

    tokio::time::sleep(SMALL_BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        3,
        "beat 4 at 1800 ms skips once the lead lower bound reaches 1.5·grant"
    );

    tokio::time::sleep(SMALL_BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        4,
        "beat 5 at 2400 ms must extend again"
    );

    guard.release("test done").await.unwrap();
}

/// A server that clamps grants (extends by only requested/4 = 1000 ms per
/// call) must still TIGHTEN the cadence to grant/2 = 500 ms — a real
/// per-extend grant is ≈ 2× the inter-success elapsed, outside the
/// lead-clamp band, so the grant regime applies. Three extends land within
/// the first 1250 ms (a held requested/2 = 2000 ms cadence would have
/// produced exactly one), and the reservation never lapses.
#[tokio::test]
async fn heartbeat_clamped_grant_still_tightens_cadence() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_clamp";
    const TTL: u64 = 4_000;

    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(initial_expiry(TTL), TTL / 4))
        .mount(&server)
        .await;

    let guard = reserve_with_ttl(&server, rsv_id, TTL).await;

    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "the first beat must be immediate"
    );

    // Grants of 1000 → cadence clamp(500, 500, 2000) = 500 ms: beats at
    // 500 and 1000 ms both extend (lead_min 500 and 1000, both < 1500).
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        3,
        "clamped per-extend grants must tighten the cadence to grant/2 = \
         500 ms (beats at 0/500/1000 ms) — not hold it at requested/2"
    );

    guard.release("test done").await.unwrap();
}

/// A server enforcing a maximum LEAD (every extend re-stamps
/// `expires_at ≈ now + L`) makes successive `expires_at_ms` differences
/// measure elapsed time, not lease. Deriving the cadence from those
/// "grants" would collapse it to the 500 ms floor within a few beats and
/// burn `max_extensions` in seconds. The lead-clamp regime instead holds
/// the cadence at min(requested/2, 30 s) = 1500 ms: over ~4.7 s the
/// heartbeat sends 4-5 extends (one extra is tolerated while the very
/// first, near-zero-elapsed beat classifies), where a floor-collapsed
/// cadence would have sent ~9.
#[tokio::test]
async fn heartbeat_lead_clamp_holds_cadence_instead_of_collapsing() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_leadclamp";
    const TTL: u64 = 3_000;

    mount_reserve(&server, rsv_id, initial_expiry(TTL)).await;
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(LeadClampEcho::holding(initial_expiry(TTL)))
        .mount(&server)
        .await;

    let guard = do_reserve(&server, TTL).await;

    // Held cadence is min(3000/2, 30 s) = 1500 ms. Depending on whether the
    // immediate first beat's few-ms "grant" rounds to zero (→ held at once)
    // or stays positive (→ one 500 ms beat before the regime engages),
    // extends land at 0/1500/3000/... or 0/500/2000/3500/...
    tokio::time::sleep(Duration::from_millis(3_250)).await;
    let after_3s = extend_calls(&server, rsv_id, TTL).await;
    assert!(
        (3..=4).contains(&after_3s),
        "lead-clamp cadence must hold at 1500 ms (3-4 extends by 3.25 s); \
         a floor-collapsed cadence would have sent ~7, got {after_3s}"
    );

    tokio::time::sleep(Duration::from_millis(1_400)).await;
    let after_4s = extend_calls(&server, rsv_id, TTL).await;
    assert!(
        (4..=5).contains(&after_4s),
        "lead-clamp cadence must stay held (4-5 extends by 4.65 s); a \
         floor-collapsed cadence would have sent ~9, got {after_4s}"
    );
    assert!(
        after_4s > after_3s,
        "the heartbeat must keep extending at the held cadence (liveness)"
    );

    guard.release("test done").await.unwrap();
}

/// A server that grants NOTHING (every extend echoes the same expiry) on
/// the immediate first beat primes the lead-clamp regime at once: the
/// zero grant must not collapse the cadence to the 500 ms floor — beats
/// hold at min(requested/2, 30 s) = 1000 ms and keep attempting (the lead
/// bound is negative, so no beat ever skips).
#[tokio::test]
async fn heartbeat_zero_grant_immediate_prime_holds_cadence() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_zerogrant";
    const TTL: u64 = 2_000;

    mount_reserve(&server, rsv_id, initial_expiry(TTL)).await;
    // Every extend returns the SAME expiry the reserve reported: grant 0.
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ACTIVE",
            "expires_at_ms": initial_expiry(TTL)
        })))
        .mount(&server)
        .await;

    let guard = do_reserve(&server, TTL).await;

    tokio::time::sleep(MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "the first beat is immediate and observes a zero grant"
    );

    // Beats hold at 1000 ms: 0/1000/2000 → exactly 3 attempts by 2.45 s.
    // A floor-collapsed cadence (500 ms) would have made ~5.
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        3,
        "zero grants must hold the cadence at min(requested/2, 30 s), \
         never collapse to the 500 ms floor"
    );

    guard.release("test done").await.unwrap();
}

/// An HTTP 200 whose status is unrecognized (forward-compat `Unknown`) still
/// counts as **applied**: a 2xx means the server DID extend, and its
/// `expires_at_ms` is authoritative. The heartbeat feeds it into the grant
/// ledger (warning only) — pinned by the fresh key on beat 2 (a failure
/// would have kept the key) and by beat 4 skipping exactly as in the
/// all-ACTIVE cadence (a failure would have left the grant uncounted and
/// beat 4 extending).
#[tokio::test]
async fn heartbeat_2xx_unknown_status_counts_as_applied() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_unknown";
    const TTL: u64 = 2_000;
    const BEAT: Duration = Duration::from_millis(1_000);

    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence {
            first_status: "SUSPENDED",
            ..ExpirySequence::granting(initial_expiry(TTL), TTL)
        })
        .mount(&server)
        .await;

    let guard = reserve_with_ttl(&server, rsv_id, TTL).await;

    tokio::time::sleep(MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "the immediate beat extends and receives the unknown-status 200"
    );

    tokio::time::sleep(BEAT).await;
    let bodies = extend_bodies(&server, rsv_id, TTL).await;
    assert_eq!(bodies.len(), 2, "beat 2 extends (lead_min 1000 < 3000)");
    assert_ne!(
        bodies[0]["idempotency_key"], bodies[1]["idempotency_key"],
        "the unknown-status 200 resolved the attempt: beat 2 must use a \
         fresh key, not retry the old one"
    );

    // Beat 3 extends (lead_min 2000); beat 4 skips (lead_min 6000 − 3000 =
    // 3000 = 1.5·grant) — proof the SUSPENDED response's grant entered the
    // ledger.
    tokio::time::sleep(BEAT + BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        3,
        "a 2xx with unknown status must count as applied: beat 4 skips"
    );

    guard.release("test done").await.unwrap();
}

// ─── Normative scheduling: remaining_ttl_ms (spec PR #148) ───────────────

/// A field-carrying server drives the schedule exactly: create reports
/// `remaining_ttl_ms = 2000`, so the first beat lands at
/// `lead_floor − retry_reserve ≈ 1000 ms` — NOT immediately (no primed
/// extension is spent) — and each extend reporting 2000 re-schedules
/// ~1000 ms after receipt. The responses also grant +2000 of expiry per
/// extend, so the background ledger accumulates enough lead that the
/// heuristic would skip beat 4 — the bypassed skip check must extend
/// every beat instead.
#[tokio::test]
async fn heartbeat_remaining_ttl_schedules_normatively_and_bypasses_skip() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_norm";
    const TTL: u64 = 2_000;
    const BEAT: Duration = Duration::from_millis(1_000);

    mount_reserve_full(&server, rsv_id, initial_expiry(TTL), Some(TTL)).await;
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(initial_expiry(TTL), TTL).with_remaining(TTL))
        .mount(&server)
        .await;

    let guard = do_reserve(&server, TTL).await;

    tokio::time::sleep(MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        0,
        "with remaining_ttl_ms on the create response there is no immediate \
         primed extension — the first beat is scheduled normatively at \
         ~1000 ms"
    );

    tokio::time::sleep(BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "beat 1 lands at lead_floor − retry_reserve ≈ 1000 ms"
    );

    // Extra 250 ms of slack: normative beats schedule from response
    // receipt, so per-beat rtt accumulates (late-only drift).
    tokio::time::sleep(BEAT + BEAT + BEAT + Duration::from_millis(250)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        4,
        "normative mode extends every ~1000 ms beat — the lead_min heuristic \
         (which would have skipped beat 4 on the accumulated +2000 grants) \
         must be bypassed"
    );

    guard.release("test done").await.unwrap();
}

/// Tenant policy caps the lease to 1 s and the create response says so:
/// `remaining_ttl_ms = 1000` puts the first beat at 500 ms — inside the
/// real lease, with no primed extension and no heuristics.
#[tokio::test]
async fn heartbeat_remaining_ttl_capped_create_first_beat_inside_lease() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_norm_cap";
    const REQUESTED: u64 = 8_000;
    const GRANTED: u64 = 1_000;

    mount_reserve_full(&server, rsv_id, initial_expiry(GRANTED), Some(GRANTED)).await;
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(
            ExpirySequence::granting(initial_expiry(GRANTED), GRANTED).with_remaining(GRANTED),
        )
        .mount(&server)
        .await;

    let guard = do_reserve(&server, REQUESTED).await;

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, REQUESTED).await,
        0,
        "no immediate prime: the first beat is scheduled at \
         reserve = min(lead/2, 1 s) = ~500 ms"
    );

    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, REQUESTED).await,
        1,
        "the first beat lands at ~500 ms — inside the real 1 s lease"
    );

    // Receipt-based ~500 ms cadence with per-beat rtt drift: 2-3 extends
    // by ~1.45 s, where the requested/2 = 4 s schedule would still show 0
    // extends after the first.
    tokio::time::sleep(Duration::from_millis(550)).await;
    let count = extend_calls(&server, rsv_id, REQUESTED).await;
    assert!(
        (2..=3).contains(&count),
        "each 1 s remaining_ttl_ms re-schedules ~500 ms after receipt \
         (got {count} by ~1.45 s)"
    );

    guard.release("test done").await.unwrap();
}

/// A maximum-LEAD-clamping server that reports `remaining_ttl_ms` needs no
/// heuristics at all: every response says ~cap remains, so beats land at
/// `cap − retry_reserve ≈ 2000 ms` — no cadence collapse to the 500 ms
/// floor, no lead-clamp regime, and no wasted primed first extension.
#[tokio::test]
async fn heartbeat_remaining_ttl_under_max_lead_clamp() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_norm_clamp";
    const TTL: u64 = 3_000;

    mount_reserve_full(&server, rsv_id, initial_expiry(TTL), Some(TTL)).await;
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(LeadClampEcho::holding(initial_expiry(TTL)).with_remaining(TTL))
        .mount(&server)
        .await;

    let guard = do_reserve(&server, TTL).await;

    tokio::time::sleep(MARGIN).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        0,
        "no primed extension under a max-lead clamp when the create reports \
         remaining_ttl_ms — the first beat waits ~2000 ms"
    );

    tokio::time::sleep(Duration::from_millis(2_000)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "beat 1 at remaining − reserve ≈ 2000 ms"
    );

    tokio::time::sleep(Duration::from_millis(2_000)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        2,
        "steady ~2000 ms cadence from the reported remaining lease — never \
         a collapse to the 500 ms floor"
    );

    guard.release("test done").await.unwrap();
}

/// The field disappears mid-flight (rollback / mixed fleet): only the
/// create and the first extend carry `remaining_ttl_ms`. The heartbeat must
/// resume the grant-ledger heuristic seamlessly — and because the ledger
/// was maintained during the normative phase, the heuristic's skip fires at
/// the right beat.
#[tokio::test]
async fn heartbeat_remaining_ttl_disappearing_falls_back_to_heuristic() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_norm_gone";
    const TTL: u64 = 1_200;

    mount_reserve_full(&server, rsv_id, initial_expiry(TTL), Some(TTL)).await;
    // Only the FIRST extend response carries the field; the rest are bare.
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(
            ExpirySequence::granting(initial_expiry(TTL), TTL)
                .with_remaining(TTL)
                .remaining_only_on_first(1),
        )
        .mount(&server)
        .await;

    let guard = do_reserve(&server, TTL).await;

    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        0,
        "normative first beat: no immediate prime"
    );

    // Normative: b1 ≈ 600 ms (lead 1200, reserve = lead/2). Its response
    // carries the field → b2 ≈ 1200 ms.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let early = extend_calls(&server, rsv_id, TTL).await;
    assert!(
        (1..=2).contains(&early),
        "normative beats at ~600/1200 ms (got {early} by 1450 ms)"
    );

    // b2's response is BARE → the heuristic resumes: grant 1200 =
    // requested → 600 ms cadence, beats at ~1800/2400/3000 ms; the ledger
    // (maintained through the normative phase, 1200 per extend) reaches
    // lead_min ≥ 1.5·grant at 3600 ms, so that beat is the heuristic skip
    // and the next extend cannot land before ~4200 ms. Exactly 5 extends
    // by 4050 ms therefore pins BOTH properties: the resumed 600 ms
    // cadence (a held/normative ~600 ms→∞ mismatch would give fewer) and
    // the skip (a skip-less 600 ms cadence would already show 6).
    tokio::time::sleep(Duration::from_millis(2_600)).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        5,
        "after the field disappears the heuristic must resume at the \
         observed 600 ms cadence AND skip at ~3600 ms on the ledger \
         maintained during the normative phase"
    );

    guard.release("test done").await.unwrap();
}

/// A transient failure in normative mode retries with the SAME idempotency
/// key after `clamp(lead_estimate/4, 1 s, 30 s)` — here lead ≈ 1000 at the
/// failed ~1000 ms beat, so the retry lands ~1 s later; after the retry
/// succeeds the next beat uses a fresh key.
#[tokio::test]
async fn heartbeat_remaining_ttl_transient_failure_retries_same_key() {
    let server = MockServer::start().await;
    let rsv_id = "rsv_hb_norm_retry";
    const TTL: u64 = 2_000;
    const BEAT: Duration = Duration::from_millis(1_000);

    mount_reserve_full(&server, rsv_id, initial_expiry(TTL), Some(TTL)).await;
    // The first extend attempt (normatively scheduled at ~1000 ms) fails...
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": "INTERNAL_ERROR",
            "message": "boom"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // ...subsequent attempts succeed, still carrying the field.
    Mock::given(method("POST"))
        .and(path(extend_path(rsv_id)))
        .respond_with(ExpirySequence::granting(initial_expiry(TTL), TTL).with_remaining(TTL))
        .mount(&server)
        .await;

    let guard = do_reserve(&server, TTL).await;

    tokio::time::sleep(MARGIN + BEAT).await;
    assert_eq!(
        extend_calls(&server, rsv_id, TTL).await,
        1,
        "the normative first beat at ~1000 ms attempts and fails"
    );

    tokio::time::sleep(BEAT).await;
    let bodies = extend_bodies(&server, rsv_id, TTL).await;
    assert_eq!(
        bodies.len(),
        2,
        "the retry lands clamp(lead/4, 1 s, 30 s) = ~1 s after the failure"
    );
    assert_eq!(
        bodies[0]["idempotency_key"], bodies[1]["idempotency_key"],
        "the normative-mode retry must reuse the failed attempt's \
         idempotency key"
    );

    tokio::time::sleep(BEAT).await;
    let bodies = extend_bodies(&server, rsv_id, TTL).await;
    assert_eq!(
        bodies.len(),
        3,
        "the next normative beat follows ~1 s later"
    );
    assert_ne!(
        bodies[2]["idempotency_key"], bodies[0]["idempotency_key"],
        "after the retry succeeds the next extend uses a fresh key"
    );

    guard.release("test done").await.unwrap();
}
