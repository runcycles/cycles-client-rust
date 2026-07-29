//! Background TTL extension (heartbeat) for active reservations.
//!
//! The protocol's `extend_by_ms` is relative to the reservation's **current
//! `expires_at_ms`**, not to request time (`cycles-protocol-v0.yaml`), so the
//! heartbeat must decide per beat whether an extension is actually needed —
//! blindly extending every beat drifts the expiry outward, and fixed
//! skip-every-other-beat cadences lapse for small TTLs or clamped grants.
//!
//! # Normative scheduling: `remaining_ttl_ms` (spec PR #148, round 5)
//!
//! Spec review round 5 proved that no client-side heuristic can decide, from
//! `(grant, elapsed)` samples alone, whether a shortfall grant is a *real*
//! per-extend lease or a maximum-LEAD clamp echoing elapsed time (sticky
//! counterexample: ttl 24 s with real +10 s grants → held cadence 12 s →
//! post-skip ratio 10/12 stays inside any plausible detection band forever
//! while the lease erodes 2 s per cycle to a lapse). The protocol therefore
//! gained a server-authoritative field: **`remaining_ttl_ms`** on both the
//! create and extend responses (spec PR #148) — the remaining reservation
//! lifetime in ms at response evaluation, same clock snapshot as
//! `expires_at_ms`, present on successful live-reservation responses.
//!
//! When a successful response carries the field, scheduling is **normative**
//! per the spec's HEARTBEAT GUIDANCE (`cycles-protocol-v0.yaml`, spec PR
//! #148; the PR's YAML is authoritative). Only a **schema-valid HTTP 200**
//! `ReservationExtendResponse` (or the create response, for the first beat)
//! counts as an observed success; any other or malformed 2xx is *ambiguous*
//! and handled as a transient failure with same-key recovery. On every
//! success, recompute from that response alone — never from accumulated
//! expiry differences:
//!
//! ```text
//! rtt            = monotonic(response_received − attempt_sent)   (per attempt)
//! lead_floor     = max(0, remaining_ttl_ms − rtt)
//! attempt_budget = max(request_timeout_budget, 1 s, 2·max_observed_rtt)
//! safety_margin  = max(1 s, 2·max_observed_rtt)
//! retry_reserve  = 2·attempt_budget + safety_margin
//! next_delay     = max(0, lead_floor − retry_reserve)
//! ```
//!
//! `request_timeout_budget` is the client's **enforced** finite per-attempt
//! bound for one complete extend call — this SDK always builds its reqwest
//! client with `.timeout(connect_timeout + read_timeout)`, so the budget is
//! always finite (an unknown/unbounded budget would be positive infinity,
//! forcing `next_delay = 0`). All arithmetic is overflow-safe saturating
//! milliseconds; budgets/margins round up, leads/delays round down. The next
//! beat is scheduled `next_delay` after response receipt. The `lead_min`
//! skip heuristic is **bypassed** while the latest success carried the field
//! (the schedule is exact; a heuristic skip could overshoot the real lease),
//! but the grant ledger below keeps running so the heuristic takes over
//! seamlessly if a later response omits the field (mixed fleets, rollbacks).
//! Same-key create/extend replays are safe to schedule from — the server
//! recomputes `remaining_ttl_ms` at replay-response construction time — and
//! `rtt` is always the *individual attempt's* own timing, never an earlier
//! attempt's.
//!
//! **Zero-delay guard:** when a schema-valid success produces
//! `next_delay = 0`, one immediate fresh-key extension may follow; if that
//! success also produces 0, the heartbeat stops and surfaces (warn) that the
//! lease is shorter than its retry-safety budget — an additive-delta server
//! gets its one immediate extension to establish a larger lead, while a
//! maximum-lead server's extension budget is never burned in a tight loop.
//!
//! **Recovery** (timeout, connection error, 5xx, 429, ambiguous 2xx):
//! `current_lead_estimate = max(0, last lead_floor − elapsed since the
//! schema-valid response that established it)`; `retry_window =
//! current_lead_estimate − attempt_budget − safety_margin` (signed, not
//! clamped). Negative window → no complete retry plus margin is provably
//! safe: stop and surface. Otherwise non-429 failures retry with the **same
//! idempotency key** after `min(30 s, lead_estimate/4, retry_window)`; a 429
//! retries after exactly `Retry-After` (delta-seconds × 1000, checked) and
//! only when that fits the window — missing/invalid/oversized `Retry-After`
//! stops rather than inventing an earlier retry that violates throttling.
//! Recovery may repeat: lead/window are recomputed from the same last
//! schema-valid response after every failure, continuing while the window
//! stays positive; a zero window permits one immediate retry, and a
//! progress guard stops the loop when the window fails to decrease between
//! consecutive failures. Any **other 4xx** stops and surfaces without
//! rotating the idempotency key. When the **create** response carries the
//! field, the first beat is scheduled from the same formula — no primed
//! extension is spent, even under a maximum-lead clamp.
//!
//! Everything below describes the **fallback** used whenever a response does
//! not carry `remaining_ttl_ms` (older servers). The fallback keeps its own
//! semantics unchanged, including any-2xx-as-applied.
//!
//! # Grant-ledger fallback (v2.3)
//!
//! ## The first beat is immediate
//!
//! A tenant policy (`max_reservation_ttl_ms`) may silently cap the granted
//! lease far below the requested TTL, and the create response carries no
//! effective-TTL field. Spec review round 4 established that **any bounded
//! first-beat delay can outlive a small capped lease** — a 30 s cap is still
//! 28 s too late for a 2 s grant. Two earlier revisions tried to *estimate*
//! the lease before the first beat (round 2: an "effective TTL" from
//! `expires_at_ms − Date`; round 3: the same difference demoted to a raw
//! cadence hint after review showed RFC 9110 `Date` is a whole-second,
//! best-effort origination timestamp from a different clock than the
//! Redis-`TIME`-stamped `expires_at_ms`). v2.3 removes estimation from the
//! scheduling path entirely: the **first extend fires immediately**. It costs
//! one extension from the allowance, but it is the only delay that provably
//! cannot outlive an arbitrarily small lease — and its response primes the
//! grant ledger with a *real* grant sample, which every later beat is paced
//! by. (`ApiResponse::date_ms` and the `httpdate` parsing remain available
//! as general response utilities; the heartbeat no longer consumes them.)
//!
//! ## Correctness: a conservative lead lower bound
//!
//! Correctness rests on `lead_min`, a rigorous **lower bound** on the expiry
//! lead (how far the server-side expiry is ahead of "now"), measured
//! *relative to the lead at heartbeat start*:
//!
//! ```text
//! lead_min = grants_sum − elapsed_ms        (signed)
//! ```
//!
//! - `grants_sum` is the sum of observed extension grants, where each grant
//!   is the difference of two **successive `expires_at_ms` values from the
//!   same server frame** (previous known expiry vs. the extend response's) —
//!   no cross-clock arithmetic anywhere;
//! - `elapsed_ms` is client-monotonic time since the heartbeat's anchor
//!   instant, taken *after* the server stamped the initial expiry, so it
//!   over-counts the server's elapsed time (conservative direction).
//!
//! `lead_min` starts at 0 (the initial grant is deliberately not counted —
//! its true size is unknowable without cross-clock math), so the true lead
//! always *exceeds* `lead_min` by the initial grant minus clock noise:
//! `lead_min` under-estimates, never over-estimates. A beat is skipped
//! **only** when the last grant is known and `lead_min ≥ 1.5 × last_grant`
//! (integer form `2·lead ≥ 3·grant`); otherwise it extends by the
//! **requested** `ttl_ms`. Until the first successful extend there is no
//! grant sample, so every beat extends — liveness first.
//!
//! ## Cadence and the lead-clamp regime
//!
//! Beat delays are computed per beat, not from a fixed interval:
//!
//! - **first beat**: immediate (see above);
//! - **after a success in the grant regime**:
//!   `clamp(grant/2, 500 ms, requested_ttl/2)` — the cadence tracks what the
//!   server actually grants (clamped grants → faster beats), floored at
//!   500 ms so tiny grants cannot busy-loop;
//! - **after a success in the LEAD-CLAMP regime**: held at
//!   `min(requested_ttl/2, 30 s)`, never tightened (see below);
//! - **after a transient failure**: retry at the current cadence with the
//!   **same idempotency key** (the server replays the original outcome, so
//!   an applied-but-lost extension cannot double-extend).
//!
//! **Why the lead-clamp regime exists** (spec review round 4): a server may
//! enforce a *maximum lead* — every extend re-stamps `expires_at ≈ now + L`
//! rather than adding lease. Under such a clamp, successive `expires_at_ms`
//! differences measure **elapsed time, not granted lease**, so deriving the
//! cadence from them is self-referential: the observed "grant" shrinks to
//! whatever the cadence is, the cadence halves in response, and within a few
//! beats it collapses to the 500 ms floor — burning the server's
//! `max_extensions` allowance in seconds. Grant-derived cadence is only
//! valid for real per-extend grants. Each success is therefore classified by
//! [`is_lead_clamp_grant`]: a grant that is non-positive, or that is both
//! well short of the requested amount (`< 0.9·requested`) and approximately
//! equal to the elapsed time since the last success (within
//! `[0.75, 1.25]·elapsed` — the signature of a clock reading rather than a
//! lease), puts the beat in the lead-clamp regime: cadence held at
//! `min(requested/2, 30 s)` and a `tracing::warn` emitted once per heartbeat
//! (the allowance is still depleting — just at the held pace, not the
//! floor's). The lower `0.75·elapsed` arm is what lets a *real* but small
//! per-extend grant recover: after a skip doubles the inter-success gap, a
//! fixed grant falls below the band and the cadence tightens again, whereas
//! a lead-clamped "grant" tracks the gap and stays inside the band. (A
//! server granting less than twice the 500 ms floor per extend is
//! observationally indistinguishable from a lead clamp at floor cadence and
//! is conservatively held too.) **The band is best-effort only** — round 5
//! proved regime detection from `(grant, elapsed)` undecidable in general
//! (sticky window: real grants in `[0.75·min(ttl/2, 30 s), 0.9·ttl)` track
//! the held cadence closely enough to stay classified lead-clamp while the
//! lease decays); `remaining_ttl_ms` above is the normative fix, and the
//! band remains only to protect legacy servers that clamp per-extend
//! deltas.
//!
//! Each next beat is scheduled from the previous beat's *intended* instant
//! (`intended + delay`), so extend round-trips do not slip the cadence; if
//! the intended next instant is already in the past (long stall, e.g. system
//! suspend), it realigns to "now" instead of bursting — the equivalent of
//! `MissedTickBehavior::Skip`. `elapsed_ms` is measured to the intended
//! instant, keeping the lead math an exact sum of the scheduled delays;
//! after a stall the realignment folds the stalled time into the next beat's
//! `elapsed_ms`, which collapses `lead_min` and forces an extend attempt
//! (whose 410, if the reservation is gone, permanently stops the task).
//!
//! Any HTTP-success (2xx) extend counts as **applied** — a 2xx means the
//! server DID apply the extension and its `expires_at_ms` is authoritative —
//! so the grant ledger is updated even if the status field is unrecognized
//! (a warning is logged). Permanent failures — `RESERVATION_EXPIRED`,
//! `RESERVATION_FINALIZED`, `MAX_EXTENSIONS_EXCEEDED`, `TENANT_CLOSED`,
//! `NOT_FOUND`, or any HTTP 410/404 — terminate the heartbeat: no further
//! extend can ever succeed, so retrying is pure noise.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::client::CyclesClient;
use crate::error::Error;
use crate::models::enums::ErrorCode;
use crate::models::{ExtendRequest, IdempotencyKey, ReservationId};

/// Hard cap on the held (lead-clamp / pre-first-sample) cadence. Even a
/// huge requested TTL beats at least every 30 s while the grant signal is
/// untrusted.
const HELD_CADENCE_CAP_MS: u64 = 30_000;

/// Floor on post-success beat delays: even zero or tiny grants retry at
/// 500 ms, never a busy loop.
const MIN_BEAT_DELAY_MS: u64 = 500;

/// The cadence held whenever the grant signal cannot be trusted:
/// `min(requested/2, 30 s)`. Used for the lead-clamp regime, and for retry
/// pacing before the first successful extend has produced a grant sample.
/// (`reserve()` validates `requested_ttl_ms ≥ 1000`, so this is ≥ 500 ms;
/// the `.max(1)` keeps it non-zero even if that invariant ever changes.)
fn held_cadence_ms(requested_ttl_ms: u64) -> u64 {
    (requested_ttl_ms / 2).clamp(1, HELD_CADENCE_CAP_MS)
}

/// `true` when a successful extend's observed grant is better explained by a
/// maximum-LEAD clamp (server re-stamps `expires_at ≈ now + L` instead of
/// adding lease) than by a real per-extend grant. Under such a clamp,
/// successive `expires_at_ms` differences measure **elapsed time, not
/// lease**, so pacing the cadence by them collapses it to the 500 ms floor
/// and burns `max_extensions` in seconds (see module docs).
///
/// Classified lead-clamp when the grant is non-positive, or is both well
/// short of the requested amount (`grant < 0.9·requested` — a full or
/// near-full grant is trusted regardless) and approximately equal to the
/// elapsed time since the last success (`0.75·elapsed ≤ grant ≤
/// 1.25·elapsed` — a clock reading, not a lease). A real grant fails the
/// band: in steady state the cadence is half the grant (`grant ≈
/// 2·elapsed`), and after a skip doubles the gap a fixed grant falls to
/// `≈ 0.5·elapsed` — below the band, so a genuinely clamped-but-real grant
/// tightens again instead of decaying at the held cadence. Integer
/// arithmetic throughout (`×10/×9`, `×4/×5`, `×4/×3`).
fn is_lead_clamp_grant(grant_ms: i128, requested_ttl_ms: u64, elapsed_ms: u128) -> bool {
    if grant_ms <= 0 {
        return true;
    }
    let elapsed = i128::try_from(elapsed_ms).unwrap_or(i128::MAX / 8);
    10 * grant_ms < 9 * i128::from(requested_ttl_ms)
        && 4 * grant_ms <= 5 * elapsed
        && 4 * grant_ms >= 3 * elapsed
}

/// Delay after a successful extend: half the observed grant, floored at
/// 500 ms (no busy loop on tiny grants) and capped at `requested/2` (a
/// server granting more than requested cannot slow the cadence below the
/// requested rhythm). `reserve()` validates `requested_ttl_ms >= 1000`, so
/// the cap is at least the floor; the `.max` keeps the clamp well-formed
/// even if that invariant ever changes.
fn next_beat_delay_ms(last_grant_ms: u64, requested_ttl_ms: u64) -> u64 {
    let hi = (requested_ttl_ms / 2).max(MIN_BEAT_DELAY_MS);
    (last_grant_ms / 2).clamp(MIN_BEAT_DELAY_MS, hi)
}

/// Lower bound on the expiry lead relative to heartbeat start, in ms
/// (signed — goes negative once elapsed time outruns the observed grants).
///
/// `grants_sum_ms` is a sum of same-server-frame expiry differences;
/// `elapsed_ms` is client-monotonic. See module docs for why this never
/// over-estimates the true lead.
fn lead_min_ms(grants_sum_ms: i128, elapsed_ms: u128) -> i128 {
    grants_sum_ms - elapsed_ms as i128
}

/// A beat is skipped only when a grant sample exists and the lead lower
/// bound is at least `1.5 · last_grant` (integer form: `2·lead ≥ 3·grant`).
/// With no sample yet (before the first successful extend) every beat
/// extends.
fn should_skip(lead_min_ms: i128, last_grant_ms: Option<u64>) -> bool {
    match last_grant_ms {
        Some(grant) => 2 * lead_min_ms >= 3 * i128::from(grant),
        None => false,
    }
}

/// `true` for extend failures that no retry can ever fix: the reservation is
/// gone (`RESERVATION_EXPIRED` / any HTTP 410, `NOT_FOUND` / any HTTP 404 —
/// a 404'd reservation never comes back), already committed or released
/// (`RESERVATION_FINALIZED`), out of extensions (`MAX_EXTENSIONS_EXCEEDED`),
/// or the owning tenant is closed (`TENANT_CLOSED` — irreversible without
/// administrative action, by which time the reservation has long expired).
fn is_permanent_extend_failure(e: &Error) -> bool {
    matches!(
        e.error_code(),
        Some(
            ErrorCode::ReservationExpired
                | ErrorCode::ReservationFinalized
                | ErrorCode::MaxExtensionsExceeded
                | ErrorCode::TenantClosed
                | ErrorCode::NotFound
        )
    ) || matches!(e.status(), Some(410 | 404))
}

/// Next intended beat instant: scheduled from the previous *intended*
/// instant (no per-beat RTT slip), realigned to "now" if that instant has
/// already passed (`MissedTickBehavior::Skip` equivalent — a stall never
/// causes a burst of catch-up beats).
fn advance(intended: tokio::time::Instant, delay: Duration) -> tokio::time::Instant {
    (intended + delay).max(tokio::time::Instant::now())
}

// ─── Normative scheduling (`remaining_ttl_ms`, spec PR #148) ─────────────
//
// Overflow-safe saturating millisecond arithmetic throughout; an unknown or
// unbounded attempt budget is `None` (positive infinity — no safe positive
// delay exists over it). Rounding direction per spec: attempt budgets and
// safety margins round up, lead lower bounds and scheduling/retry delays
// round down (`ceil_ms` rounds the *consumed* rtt and elapsed measurements
// up, which rounds the derived leads down).

/// A `Duration` in whole milliseconds, rounded **up** — for round trips and
/// elapsed-time measurements that are *subtracted* from leases, so rounding
/// can never fabricate lease.
pub(crate) fn ceil_ms(d: Duration) -> u64 {
    u64::try_from(d.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX)
}

/// Floor on the lease actually left at response receipt: the server
/// evaluated `remaining_ttl_ms` before the response travelled back, so at
/// receipt the lease has at most `rtt` (the full round trip — an upper
/// bound on the return leg) less remaining.
fn lead_floor_ms(remaining_ttl_ms: u64, rtt_ms: u64) -> u64 {
    remaining_ttl_ms.saturating_sub(rtt_ms)
}

/// Upper bound on one complete extend attempt (connect, write, read, and
/// failure detection): `max(request_timeout_budget, 1 s, 2·max_observed
/// rtt)`. `None` (unknown/unbounded per-attempt timeout) is positive
/// infinity.
fn attempt_budget_ms(request_timeout_ms: Option<u64>, max_rtt_ms: u64) -> Option<u64> {
    request_timeout_ms.map(|t| t.max(1_000).max(max_rtt_ms.saturating_mul(2)))
}

/// Scheduling/network slack on top of the attempt budget:
/// `max(1 s, 2·max_observed rtt)`.
fn safety_margin_ms(max_rtt_ms: u64) -> u64 {
    max_rtt_ms.saturating_mul(2).max(1_000)
}

/// Lease reserved for recovery — one failed attempt, one same-key retry,
/// and margin: `2·attempt_budget + safety_margin`. `None` = infinity.
fn retry_reserve_ms(request_timeout_ms: Option<u64>, max_rtt_ms: u64) -> Option<u64> {
    attempt_budget_ms(request_timeout_ms, max_rtt_ms).map(|b| {
        b.saturating_mul(2)
            .saturating_add(safety_margin_ms(max_rtt_ms))
    })
}

/// Delay from response receipt to the next beat in normative mode:
/// `max(0, lead_floor − retry_reserve)`. Zero when the lease cannot hold
/// the full recovery reserve (the zero-delay guard bounds how often that
/// may recur) or when the attempt budget is unbounded.
fn remaining_next_delay_ms(
    remaining_ttl_ms: u64,
    rtt_ms: u64,
    request_timeout_ms: Option<u64>,
    max_rtt_ms: u64,
) -> u64 {
    match retry_reserve_ms(request_timeout_ms, max_rtt_ms) {
        Some(reserve) => lead_floor_ms(remaining_ttl_ms, rtt_ms).saturating_sub(reserve),
        None => 0,
    }
}

/// Signed recovery window after a transient failure in normative mode:
/// `current_lead_estimate − attempt_budget − safety_margin`, WITHOUT
/// clamping — negative means no complete retry plus margin is provably safe
/// and the client must stop. `None` when the attempt budget is unbounded
/// (equally: no provably safe retry).
fn recovery_window_ms(
    lead_estimate_ms: u64,
    request_timeout_ms: Option<u64>,
    max_rtt_ms: u64,
) -> Option<i128> {
    attempt_budget_ms(request_timeout_ms, max_rtt_ms).map(|budget| {
        i128::from(lead_estimate_ms) - i128::from(budget) - i128::from(safety_margin_ms(max_rtt_ms))
    })
}

/// Same-key retry delay for a non-429 transient failure when the recovery
/// window is non-negative: `min(30 s, lead_estimate/4, retry_window)`. A
/// zero window retries immediately — once; the progress guard stops a
/// second zero-window failure.
fn recovery_delay_ms(lead_estimate_ms: u64, window_ms: i128) -> u64 {
    let window = u64::try_from(window_ms).unwrap_or(0);
    30_000.min(lead_estimate_ms / 4).min(window)
}

/// Progress guard for repeated recovery: `true` when the freshly recomputed
/// window failed to DECREASE since the previous failure of the same
/// recovery run — the window shrinks exactly when monotonic elapsed grows,
/// so a non-decreasing window means a zero-time recovery loop (including
/// the second failure at a coarse-clock window of 0), which must stop.
fn recovery_stalled(
    window_ms: i128,
    previous_window_ms: Option<i128>,
    elapsed_advanced: bool,
) -> bool {
    previous_window_ms.is_some_and(|prev| !elapsed_advanced && window_ms >= prev)
}

/// `true` for failure classes the primary (field-mode) algorithm recovers
/// from with the same idempotency key: timeout/connection errors
/// (`Transport`), ambiguous 2xx (`Deserialization` — a schema-invalid or
/// non-200 2xx body), 5xx, and 429. Everything else — notably other 4xx —
/// stops and surfaces without key rotation (permanent codes are classified
/// before this).
fn is_recoverable_in_field_mode(e: &Error) -> bool {
    matches!(e, Error::Transport(_) | Error::Deserialization(_))
        || matches!(e.status(), Some(s) if s >= 500 || s == 429)
}

/// Spawn a background task that periodically extends a reservation's TTL.
///
/// `requested_ttl_ms` is the TTL the caller asked for at reserve time. It is
/// the per-beat `extend_by_ms` and bounds the beat cadence; the actual
/// cadence adapts to the grants the server is observed to make (see module
/// docs).
///
/// `initial_expires_at_ms` is the `expires_at_ms` from the reserve response
/// (server frame) — the base of the grant ledger. If the server omitted it
/// (non-conformant — the spec requires it on allowed non-dry-run
/// reservations), the first grant cannot be measured and falls back to the
/// requested amount.
///
/// `initial_remaining_ttl_ms` is the create response's `remaining_ttl_ms`
/// (spec PR #148), and `create_rtt_ms` the reserve call's measured round
/// trip. `create_received_at` anchors the sample so local guard setup time is
/// also deducted before scheduling. When the field is present, the first beat and all subsequent
/// scheduling are **normative** (exact, server-authoritative); when absent,
/// the first beat fires **immediately** — the only first-beat delay that
/// provably cannot outlive an arbitrarily small tenant-capped lease (see
/// module docs).
///
/// Returns a `JoinHandle` that can be used to await the task. Cancel the
/// provided `CancellationToken` to stop the heartbeat. The task also stops
/// itself on a permanent extend failure (see
/// [`is_permanent_extend_failure`]).
pub(crate) struct CreateLeaseSample {
    pub(crate) remaining_ttl_ms: Option<u64>,
    pub(crate) rtt_ms: u64,
    pub(crate) received_at: tokio::time::Instant,
}

pub(crate) fn start_heartbeat(
    client: CyclesClient,
    reservation_id: ReservationId,
    requested_ttl_ms: u64,
    initial_expires_at_ms: Option<u64>,
    create_sample: CreateLeaseSample,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let initial_remaining_ttl_ms = create_sample.remaining_ttl_ms;
        let create_rtt_ms = create_sample.rtt_ms;
        let create_received_at = create_sample.received_at;
        // Monotonic anchor for elapsed_ms. Taken when the heartbeat starts,
        // i.e. shortly AFTER the server stamped initial_expires_at_ms — so
        // elapsed_ms over-counts server elapsed time and lead_min stays a
        // lower bound.
        let anchor = tokio::time::Instant::now();
        // Fallback cadence used after a failure before any grant sample
        // exists (fallback mode only).
        let mut delay = Duration::from_millis(held_cadence_ms(requested_ttl_ms));

        // The client's ENFORCED finite per-attempt bound for one complete
        // extend call: the reqwest client is built with
        // `.timeout(connect_timeout + read_timeout)`. Rounded up (budget).
        // An unknown/unbounded budget would be None (positive infinity).
        let request_timeout_ms: Option<u64> = Some(ceil_ms(
            client.config().connect_timeout + client.config().read_timeout,
        ));

        // Normative-mode state: true while the most recent schema-valid
        // success carried remaining_ttl_ms; lead_sample is that response's
        // (lead_floor, receipt instant) for recovery lead estimates.
        let mut normative = initial_remaining_ttl_ms.is_some();
        let mut max_rtt_ms = create_rtt_ms;
        let elapsed_after_receipt_ms =
            ceil_ms(anchor.saturating_duration_since(create_received_at));
        let remaining_at_start =
            initial_remaining_ttl_ms.map(|r| r.saturating_sub(elapsed_after_receipt_ms));
        let mut lead_sample: Option<(u64, tokio::time::Instant)> =
            remaining_at_start.map(|r| (lead_floor_ms(r, create_rtt_ms), anchor));
        // Consecutive schema-valid successes whose next_delay was 0 (the
        // field-carrying create counts). Two in a row → the lease is
        // shorter than the retry-safety budget → stop.
        let mut zero_delay_streak: u32 = 0;
        // Previous failure's recovery window in the current recovery run
        // (None outside recovery): the progress guard stops the run when
        // the window fails to decrease between consecutive failures.
        let mut last_recovery_window: Option<i128> = None;
        let mut last_recovery_failure_at: Option<tokio::time::Instant> = None;
        let mut zero_window_retried = false;

        // The upcoming beat's *intended* instant; lead math measures elapsed
        // to this. Normative when the create response carried the field,
        // otherwise the immediate prime.
        let mut next_beat = match remaining_at_start {
            Some(remaining) => {
                let nd = remaining_next_delay_ms(
                    remaining,
                    create_rtt_ms,
                    request_timeout_ms,
                    max_rtt_ms,
                );
                if nd == 0 {
                    // Zero-delay guard, first arm: the create's lease cannot
                    // hold the recovery reserve — one immediate fresh-key
                    // extension is permitted.
                    zero_delay_streak = 1;
                }
                anchor + Duration::from_millis(nd)
            }
            None => anchor,
        };

        // Grant ledger: previous known server-frame expiry, sum of observed
        // grants, and the most recent grant (None until the first success).
        // Maintained in BOTH modes so the fallback heuristic can take over
        // if a response stops carrying remaining_ttl_ms.
        let mut prev_expiry = initial_expires_at_ms;
        let mut grants_sum_ms: i128 = 0;
        let mut last_grant_ms: Option<u64> = None;
        // Intended instant of the last successful extend (initially the
        // anchor): the elapsed base for lead-clamp classification.
        let mut last_success = anchor;
        // The lead-clamp warning fires once per heartbeat, not per beat.
        let mut warned_lead_clamp = false;
        // Set while an extend outcome is unresolved (transient failure):
        // reused on the retry so a lost response cannot double-extend.
        let mut pending_key: Option<IdempotencyKey> = None;

        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep_until(next_beat) => {}
            }

            // The skip heuristic applies only in fallback mode: normative
            // scheduling is exact, and a heuristic skip on top of it could
            // overshoot the real lease.
            let elapsed_ms = next_beat.duration_since(anchor).as_millis();
            if !normative && should_skip(lead_min_ms(grants_sum_ms, elapsed_ms), last_grant_ms) {
                next_beat = advance(next_beat, delay);
                continue;
            }

            let key = pending_key.clone().unwrap_or_else(IdempotencyKey::random);
            let req = ExtendRequest {
                idempotency_key: key.clone(),
                extend_by_ms: requested_ttl_ms,
                metadata: None,
            };
            let sent_at = tokio::time::Instant::now();
            // In both scheduling modes only a schema-valid HTTP 200 counts
            // as observed success. A different or malformed 2xx is
            // ambiguous and must retain the same idempotency key.
            let result = client
                .extend_reservation_strict(&reservation_id, &req)
                .await;
            match result {
                Ok(resp) => {
                    // Capture receipt immediately after the complete strict
                    // response validation. Both RTT and the lead-floor anchor
                    // must use this same instant; anchoring a little later
                    // would credit local processing time back into the lease.
                    let received_at = tokio::time::Instant::now();
                    // Per-attempt rtt (never an earlier attempt's timing —
                    // same-key replays recompute remaining_ttl_ms server
                    // side), rounded up: it is subtracted from the lease.
                    let rtt_ms = ceil_ms(received_at.duration_since(sent_at));
                    max_rtt_ms = max_rtt_ms.max(rtt_ms);
                    pending_key = None;
                    last_recovery_window = None;
                    last_recovery_failure_at = None;
                    zero_window_retried = false;
                    // The grant is the difference of successive server-frame
                    // expiries (signed — an expiry that moved backwards is a
                    // negative grant, classified lead-clamp and counted as
                    // zero in the ledger). `ExtendResponse::expires_at_ms`
                    // is structurally required, so the only possible missing
                    // sample is a reserve response without expires_at_ms
                    // (non-conformant server): fall back to the requested
                    // amount.
                    let grant: i128 = match prev_expiry {
                        Some(prev) => i128::from(resp.expires_at_ms) - i128::from(prev),
                        None => i128::from(requested_ttl_ms),
                    };
                    prev_expiry = Some(resp.expires_at_ms);
                    let elapsed_since_success = next_beat.duration_since(last_success).as_millis();
                    last_success = next_beat;
                    // Ledger counts max(grant, 0); the skip rule and cadence
                    // never see negative amounts. Maintained even in
                    // normative mode so the fallback can resume seamlessly.
                    let counted = u64::try_from(grant.max(0)).unwrap_or(u64::MAX);
                    grants_sum_ms += i128::from(counted);
                    last_grant_ms = Some(counted);
                    match resp.remaining_ttl_ms {
                        Some(remaining) => {
                            // Normative: the server told us exactly how much
                            // lease is left; schedule next_delay after
                            // receipt. Expiry-difference heuristics are not
                            // used for scheduling in this mode.
                            normative = true;
                            let floor = lead_floor_ms(remaining, rtt_ms);
                            lead_sample = Some((floor, received_at));
                            let nd = remaining_next_delay_ms(
                                remaining,
                                rtt_ms,
                                request_timeout_ms,
                                max_rtt_ms,
                            );
                            if nd == 0 {
                                // Zero-delay guard: one immediate FRESH-key
                                // extension may follow a zero-delay success
                                // (an additive-delta server can establish a
                                // larger lead with it); a second zero-delay
                                // success in a row proves the lease is
                                // shorter than the retry-safety budget —
                                // stop rather than burn a maximum-lead
                                // server's extension budget in a tight loop.
                                zero_delay_streak += 1;
                                if zero_delay_streak >= 2 {
                                    tracing::warn!(
                                        reservation_id = %reservation_id,
                                        remaining_ttl_ms = remaining,
                                        retry_reserve_ms =
                                            ?retry_reserve_ms(request_timeout_ms, max_rtt_ms),
                                        "reservation lease is shorter than the heartbeat's retry-safety budget (next_delay = 0 twice in a row); stopping heartbeat"
                                    );
                                    break;
                                }
                                next_beat = received_at;
                            } else {
                                zero_delay_streak = 0;
                                next_beat = received_at + Duration::from_millis(nd);
                            }
                        }
                        None => {
                            // Fallback: v2.3 grant-ledger regime.
                            normative = false;
                            zero_delay_streak = 0;
                            if is_lead_clamp_grant(grant, requested_ttl_ms, elapsed_since_success) {
                                // The observed "grant" tracks elapsed time,
                                // not the requested lease: pacing by it
                                // would collapse the cadence to the floor
                                // and burn max_extensions in seconds. Hold
                                // at min(requested/2, 30 s) instead.
                                delay = Duration::from_millis(held_cadence_ms(requested_ttl_ms));
                                if !warned_lead_clamp {
                                    warned_lead_clamp = true;
                                    tracing::warn!(
                                        reservation_id = %reservation_id,
                                        grant_ms = %grant,
                                        elapsed_ms = %elapsed_since_success,
                                        requested_ttl_ms,
                                        held_cadence_ms = held_cadence_ms(requested_ttl_ms),
                                        "extend grants track elapsed time, not the requested lease — server appears to clamp the reservation's maximum lead; holding heartbeat cadence to avoid depleting the extension allowance"
                                    );
                                }
                            } else {
                                delay = Duration::from_millis(next_beat_delay_ms(
                                    counted,
                                    requested_ttl_ms,
                                ));
                            }
                            next_beat = advance(next_beat, delay);
                        }
                    }
                }
                Err(e) if is_permanent_extend_failure(&e) => {
                    tracing::warn!(
                        reservation_id = %reservation_id,
                        error = %e,
                        "heartbeat extend failed permanently; stopping heartbeat"
                    );
                    break;
                }
                Err(e) if normative => {
                    // Primary-path recovery per the spec's HEARTBEAT
                    // GUIDANCE. Classes: timeout/connection error, 5xx, 429,
                    // ambiguous 2xx → same-key recovery bounded by the
                    // retry window; any other 4xx → stop and surface
                    // WITHOUT rotating the idempotency key.
                    if !is_recoverable_in_field_mode(&e) {
                        tracing::warn!(
                            reservation_id = %reservation_id,
                            error = %e,
                            "heartbeat extend rejected (non-retryable request/authorization failure); stopping heartbeat without key rotation"
                        );
                        break;
                    }
                    // current_lead_estimate from the last schema-valid
                    // response — elapsed rounded up (it is consumed lease).
                    let failure_at = tokio::time::Instant::now();
                    let lead_now = lead_sample.map_or(0, |(floor, at)| {
                        floor.saturating_sub(ceil_ms(failure_at.duration_since(at)))
                    });
                    // An unbounded attempt budget (None) admits no provably
                    // safe retry either: fold it into the negative window.
                    let window = recovery_window_ms(lead_now, request_timeout_ms, max_rtt_ms)
                        .unwrap_or(i128::MIN);
                    if window == 0 && zero_window_retried {
                        tracing::warn!(
                            reservation_id = %reservation_id,
                            error = %e,
                            "heartbeat retry window is still zero after the one permitted immediate recovery retry; stopping heartbeat"
                        );
                        break;
                    }
                    // Progress guard: the window must decrease between
                    // consecutive failures of the same recovery run (it
                    // decreases exactly when monotonic elapsed grows) — this
                    // also enforces "one immediate retry at window 0, then
                    // stop".
                    let elapsed_advanced =
                        last_recovery_failure_at.is_none_or(|previous| failure_at > previous);
                    if recovery_stalled(window, last_recovery_window, elapsed_advanced) {
                        tracing::warn!(
                            reservation_id = %reservation_id,
                            error = %e,
                            "heartbeat recovery made no progress between consecutive failures; stopping heartbeat"
                        );
                        break;
                    }
                    if window < 0 {
                        tracing::warn!(
                            reservation_id = %reservation_id,
                            error = %e,
                            lead_estimate_ms = lead_now,
                            "no complete extend retry plus safety margin fits the remaining lease; stopping heartbeat (lease cannot be safely renewed)"
                        );
                        break;
                    }
                    if window == 0 {
                        zero_window_retried = true;
                    }
                    let retry_delay_ms = if e.status() == Some(429) {
                        // 429: retry after exactly Retry-After (delta-seconds
                        // × 1000, parsed overflow-safe by the error layer),
                        // and only when it fits the retry window — never
                        // invent an earlier retry that violates throttling.
                        match e.retry_after().map(ceil_ms) {
                            Some(ra_ms) if i128::from(ra_ms) <= window => ra_ms,
                            ra => {
                                tracing::warn!(
                                    reservation_id = %reservation_id,
                                    error = %e,
                                    retry_after_ms = ?ra,
                                    "429 Retry-After is missing, invalid, or exceeds the safe retry window; stopping heartbeat (lease cannot be safely renewed)"
                                );
                                break;
                            }
                        }
                    } else {
                        recovery_delay_ms(lead_now, window)
                    };
                    // Same key: the retry must dedupe against a possibly
                    // applied-but-lost extension.
                    pending_key = Some(key);
                    last_recovery_window = Some(window);
                    last_recovery_failure_at = Some(failure_at);
                    next_beat = tokio::time::Instant::now() + Duration::from_millis(retry_delay_ms);
                    warn_field_retry(&reservation_id, &e, retry_delay_ms);
                }
                Err(e) => {
                    // Fallback: keep the key (the retry must dedupe against
                    // a possibly applied-but-lost extension) and retry at
                    // the current cadence.
                    pending_key = Some(key);
                    next_beat = advance(next_beat, delay);
                    warn_fallback_retry(&reservation_id, &e);
                }
            }
        }
    })
}

fn warn_field_retry(reservation_id: &ReservationId, error: &crate::Error, retry_delay_ms: u64) {
    tracing::warn!(
        reservation_id = %reservation_id,
        error = %error,
        retry_delay_ms,
        "heartbeat extend failed; retrying with the same idempotency key within the recovery window"
    );
}

fn warn_fallback_retry(reservation_id: &ReservationId, error: &crate::Error) {
    tracing::warn!(
        reservation_id = %reservation_id,
        error = %error,
        "heartbeat extend failed; retrying next beat with the same idempotency key"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUESTED: u64 = 2_000;

    #[test]
    #[tracing_test::traced_test]
    fn fallback_retry_warning_includes_reservation_and_disposition() {
        warn_fallback_retry(
            &ReservationId::new("rsv-observable"),
            &crate::Error::Validation("socket timed out".to_string()),
        );
        assert!(logs_contain("rsv-observable"));
        assert!(logs_contain(
            "heartbeat extend failed; retrying next beat with the same idempotency key"
        ));
    }

    #[test]
    fn lead_min_starts_at_zero_and_goes_negative() {
        // No grants yet: the bound is exactly −elapsed (signed, no wrap).
        assert_eq!(lead_min_ms(0, 0), 0);
        assert_eq!(lead_min_ms(0, 750), -750);
        // Grants accumulate against elapsed time.
        assert_eq!(lead_min_ms(4_000, 3_000), 1_000);
        assert_eq!(lead_min_ms(2_000, 10_000), -8_000);
    }

    #[test]
    fn no_skip_before_first_grant_sample() {
        // Whatever the bound says, without a grant sample every beat
        // extends (liveness first).
        assert!(!should_skip(i128::MAX / 4, None));
        assert!(!should_skip(0, None));
    }

    #[test]
    fn skip_threshold_is_1_5_times_last_grant_inclusive() {
        // grant 2000 → threshold 3000, inclusive.
        assert!(should_skip(3_000, Some(REQUESTED)));
        assert!(!should_skip(2_999, Some(REQUESTED)));
        // Negative lead never skips.
        assert!(!should_skip(-1, Some(REQUESTED)));
        // Zero grant: threshold 0 — skips only while the bound is
        // non-negative, which decays as elapsed grows.
        assert!(should_skip(0, Some(0)));
        assert!(!should_skip(-1, Some(0)));
    }

    #[test]
    fn full_grant_trace_extends_three_beats_then_alternates() {
        // grants == requested, immediate first beat then 1000 ms beats
        // (beat i fires at (i−1)·1000): extend@1..3 (bounds 0, 1000, 2000
        // against the 1.5·grant = 3000 threshold), skip@4 (bound exactly
        // 3000, inclusive), extend@5, skip@6 — the v2.3 steady state.
        let mut grants: i128 = 0;
        let mut extends = Vec::new();
        for beat in 1u128..=6 {
            let lead = lead_min_ms(grants, (beat - 1) * 1_000);
            let skip = should_skip(lead, if beat == 1 { None } else { Some(REQUESTED) });
            extends.push(!skip);
            if !skip {
                grants += i128::from(REQUESTED);
            }
        }
        assert_eq!(extends, [true, true, true, false, true, false]);
    }

    #[test]
    fn held_cadence_pins() {
        // requested/2 below the cap.
        assert_eq!(held_cadence_ms(2_000), 1_000);
        // Spec-minimum TTL.
        assert_eq!(held_cadence_ms(1_000), 500);
        // Huge requested TTL → the 30 s cap. (Impractical to wait out in a
        // wall-clock test — pinned here.)
        assert_eq!(held_cadence_ms(86_400_000), 30_000);
        assert_eq!(held_cadence_ms(60_000), 30_000);
        // Degenerate sub-spec TTL stays non-zero.
        assert_eq!(held_cadence_ms(1), 1);
    }

    #[test]
    fn lead_clamp_zero_or_negative_grant_always_holds() {
        // A zero grant on the immediate first beat (elapsed 0) is the
        // canonical lead-clamp prime: hold, never derive a cadence from it.
        assert!(is_lead_clamp_grant(0, REQUESTED, 0));
        // Expiry moved backwards.
        assert!(is_lead_clamp_grant(-500, REQUESTED, 1_000));
        assert!(is_lead_clamp_grant(0, REQUESTED, 10_000));
    }

    #[test]
    fn lead_clamp_full_grant_is_trusted_regardless_of_elapsed() {
        // grant ≥ 0.9·requested is never classified lead-clamp, even when
        // it happens to equal elapsed (e.g. a retry after a long outage).
        assert!(!is_lead_clamp_grant(2_000, REQUESTED, 2_000));
        // Boundary: exactly 0.9·requested is trusted (strict <).
        assert!(!is_lead_clamp_grant(1_800, REQUESTED, 1_800));
        // Just below 0.9·requested inside the band is not.
        assert!(is_lead_clamp_grant(1_799, REQUESTED, 1_799));
    }

    #[test]
    fn lead_clamp_band_is_0_75_to_1_25_of_elapsed() {
        // The signature of a maximum-lead clamp: the "grant" tracks elapsed
        // time. requested 8000 so the 0.9·requested arm is inactive.
        const REQ: u64 = 8_000;
        // Ratio 1.0 → clamp.
        assert!(is_lead_clamp_grant(1_000, REQ, 1_000));
        // Upper edge inclusive: grant = 1.25·elapsed.
        assert!(is_lead_clamp_grant(1_250, REQ, 1_000));
        assert!(!is_lead_clamp_grant(1_251, REQ, 1_000));
        // Lower edge inclusive: grant = 0.75·elapsed.
        assert!(is_lead_clamp_grant(750, REQ, 1_000));
        // Below the band: a REAL fixed grant observed across a skip-doubled
        // gap (grant ≈ 0.5·elapsed) must NOT hold — it tightens again.
        assert!(!is_lead_clamp_grant(749, REQ, 1_000));
        assert!(!is_lead_clamp_grant(1_000, REQ, 2_000));
        // Steady-state real grant (cadence = grant/2 → grant = 2·elapsed).
        assert!(!is_lead_clamp_grant(2_000, REQ, 1_000));
        // Immediate first beat (elapsed 0): any positive grant is taken at
        // face value — the regime is re-evaluated on the next beat.
        assert!(!is_lead_clamp_grant(5, REQ, 0));
    }

    #[test]
    fn next_beat_delay_tracks_grant_within_bounds() {
        // Full grant → grant/2.
        assert_eq!(next_beat_delay_ms(2_000, 2_000), 1_000);
        // Clamped grant → faster cadence, floored at 500 ms.
        assert_eq!(next_beat_delay_ms(500, 2_000), 500);
        assert_eq!(next_beat_delay_ms(0, 2_000), 500);
        // Over-grant → capped at requested/2 (never slower than the
        // requested rhythm).
        assert_eq!(next_beat_delay_ms(20_000, 8_000), 4_000);
        // Capped-grant scenario from the wiremock suite: requested 8000,
        // grants of 2000 → 1000 ms beats.
        assert_eq!(next_beat_delay_ms(2_000, 8_000), 1_000);
    }

    #[test]
    fn lead_floor_subtracts_rtt_and_saturates() {
        assert_eq!(lead_floor_ms(2_000, 3), 1_997);
        assert_eq!(lead_floor_ms(2_000, 0), 2_000);
        // rtt exceeding the reported remaining → floor 0, never wraps.
        assert_eq!(lead_floor_ms(100, 200), 0);
    }

    #[test]
    fn ceil_ms_rounds_up() {
        assert_eq!(ceil_ms(Duration::from_millis(5)), 5);
        assert_eq!(ceil_ms(Duration::from_micros(1)), 1);
        assert_eq!(ceil_ms(Duration::from_micros(4_001)), 5);
        assert_eq!(ceil_ms(Duration::ZERO), 0);
    }

    #[test]
    fn attempt_budget_and_safety_margin_pins() {
        // Budget: max(request timeout, 1 s, 2·max_rtt) — round up, never
        // below the 1 s floor.
        assert_eq!(attempt_budget_ms(Some(500), 0), Some(1_000));
        assert_eq!(attempt_budget_ms(Some(10_000), 1_500), Some(10_000));
        assert_eq!(attempt_budget_ms(Some(1_000), 5_000), Some(10_000));
        // Unknown/unbounded request timeout → positive infinity.
        assert_eq!(attempt_budget_ms(None, 5_000), None);
        // Safety margin: max(1 s, 2·max_rtt).
        assert_eq!(safety_margin_ms(0), 1_000);
        assert_eq!(safety_margin_ms(400), 1_000);
        assert_eq!(safety_margin_ms(1_500), 3_000);
    }

    #[test]
    fn remaining_next_delay_pins() {
        // Spec worked example: lead ≈ 60 000, 10 s request timeout, 1.5 s
        // max rtt → attempt_budget 10 000, safety 3 000, reserve 23 000 →
        // next_delay 37 000.
        assert_eq!(
            remaining_next_delay_ms(60_000, 0, Some(10_000), 1_500),
            37_000
        );
        // Spec worked example: a 30 s timeout makes the reserve ≥ 61 000,
        // so a 60 000 lead produces 0.
        assert_eq!(remaining_next_delay_ms(60_000, 0, Some(30_000), 0), 0);
        // Minimal budgets (small enforced timeout, negligible rtt):
        // reserve = 2·1000 + 1000 = 3000.
        assert_eq!(remaining_next_delay_ms(60_000, 0, Some(500), 0), 57_000);
        assert_eq!(remaining_next_delay_ms(4_000, 0, Some(500), 0), 1_000);
        assert_eq!(remaining_next_delay_ms(3_000, 0, Some(500), 0), 0);
        // rtt shaves the lead floor first.
        assert_eq!(remaining_next_delay_ms(4_000, 500, Some(500), 0), 500);
        // Unknown/unbounded attempt budget → 0 (never a positive delay).
        assert_eq!(remaining_next_delay_ms(3_600_000, 0, None, 0), 0);
        // Zero/exhausted lease → 0, never wraps.
        assert_eq!(remaining_next_delay_ms(0, 0, Some(500), 0), 0);
        assert_eq!(remaining_next_delay_ms(100, 200, Some(500), 0), 0);
    }

    #[test]
    fn recovery_window_pins() {
        // window = lead − attempt_budget − safety_margin, signed, unclamped.
        assert_eq!(recovery_window_ms(2_985, Some(500), 0), Some(985));
        assert_eq!(recovery_window_ms(2_000, Some(500), 0), Some(0));
        // Negative window: no complete retry plus margin fits.
        assert_eq!(recovery_window_ms(1_997, Some(500), 0), Some(-3));
        assert_eq!(recovery_window_ms(0, Some(500), 0), Some(-2_000));
        // Slow rtts widen both the budget and the margin.
        assert_eq!(recovery_window_ms(10_000, Some(1_000), 1_500), Some(4_000));
        // Unbounded attempt budget → no provably safe retry.
        assert_eq!(recovery_window_ms(u64::MAX, None, 0), None);
    }

    #[test]
    fn recovery_delay_is_min_of_cap_quarter_lead_and_window() {
        // min(30 s, lead/4, window).
        assert_eq!(recovery_delay_ms(2_985, 985), 746);
        assert_eq!(recovery_delay_ms(8_000, 30_000), 2_000);
        assert_eq!(recovery_delay_ms(400_000, 200_000), 30_000);
        // Zero window → immediate retry (once; the progress guard stops a
        // second zero-window failure).
        assert_eq!(recovery_delay_ms(2_000, 0), 0);
    }

    #[test]
    fn recovery_progress_guard() {
        // First failure of a run: no previous window → proceed.
        assert!(!recovery_stalled(985, None, false));
        assert!(!recovery_stalled(0, None, false));
        // Window decreased → progress → proceed.
        assert!(!recovery_stalled(240, Some(985), false));
        assert!(!recovery_stalled(-3, Some(240), false));
        // Raw monotonic elapsed advanced even if conservative millisecond
        // rounding left the integer window unchanged.
        assert!(!recovery_stalled(985, Some(985), true));
        // Window failed to decrease (coarse clock / zero-time loop) → stop.
        assert!(recovery_stalled(985, Some(985), false));
        assert!(recovery_stalled(986, Some(985), false));
        // Second failure at a zero window → stop (one immediate retry only).
        assert!(recovery_stalled(0, Some(0), false));
    }

    #[test]
    fn field_mode_recoverable_classification() {
        let api = |status: u16| Error::Api {
            status,
            code: None,
            message: "x".into(),
            request_id: None,
            retry_after: None,
            details: None,
        };
        // 5xx and 429 recover with the same key.
        assert!(is_recoverable_in_field_mode(&api(500)));
        assert!(is_recoverable_in_field_mode(&api(503)));
        assert!(is_recoverable_in_field_mode(&api(429)));
        // Ambiguous 2xx surfaces as Deserialization → recoverable.
        assert!(is_recoverable_in_field_mode(&Error::Deserialization(
            serde::de::Error::custom("ambiguous")
        )));
        // Other 4xx: stop and surface, no key rotation.
        assert!(!is_recoverable_in_field_mode(&api(400)));
        assert!(!is_recoverable_in_field_mode(&api(401)));
        assert!(!is_recoverable_in_field_mode(&api(403)));
        assert!(!is_recoverable_in_field_mode(&api(422)));
        // Non-HTTP oddities are not silently retried either.
        assert!(!is_recoverable_in_field_mode(&Error::Validation(
            "x".into()
        )));
    }

    #[test]
    fn permanent_failure_classification() {
        let api = |status: u16, code: Option<ErrorCode>| Error::Api {
            status,
            code,
            message: "x".into(),
            request_id: None,
            retry_after: None,
            details: None,
        };
        assert!(is_permanent_extend_failure(&api(
            410,
            Some(ErrorCode::ReservationExpired)
        )));
        assert!(is_permanent_extend_failure(&api(
            409,
            Some(ErrorCode::ReservationFinalized)
        )));
        assert!(is_permanent_extend_failure(&api(
            409,
            Some(ErrorCode::MaxExtensionsExceeded)
        )));
        // 410 with an unparseable body (no typed code) is still permanent.
        assert!(is_permanent_extend_failure(&api(410, None)));
        // Tenant closure is irreversible; a 404'd reservation never returns.
        assert!(is_permanent_extend_failure(&api(
            409,
            Some(ErrorCode::TenantClosed)
        )));
        assert!(is_permanent_extend_failure(&api(
            404,
            Some(ErrorCode::NotFound)
        )));
        assert!(is_permanent_extend_failure(&api(404, None)));
        // Transient shapes are not.
        assert!(!is_permanent_extend_failure(&api(
            500,
            Some(ErrorCode::InternalError)
        )));
        assert!(!is_permanent_extend_failure(&api(429, None)));
        assert!(!is_permanent_extend_failure(&Error::Validation("x".into())));
    }
}
