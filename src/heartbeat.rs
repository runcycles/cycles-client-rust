//! Background TTL extension (heartbeat) for active reservations.
//!
//! # Grant-ledger design (v2.3)
//!
//! The protocol's `extend_by_ms` is relative to the reservation's **current
//! `expires_at_ms`**, not to request time (`cycles-protocol-v0.yaml`), so the
//! heartbeat must decide per beat whether an extension is actually needed —
//! blindly extending every beat drifts the expiry outward, and fixed
//! skip-every-other-beat cadences lapse for small TTLs or clamped grants.
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
//! is conservatively held too.)
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
use crate::models::enums::{ErrorCode, ExtendStatus};
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
/// The first beat fires **immediately** — the only first-beat delay that
/// provably cannot outlive an arbitrarily small tenant-capped lease (see
/// module docs).
///
/// Returns a `JoinHandle` that can be used to await the task. Cancel the
/// provided `CancellationToken` to stop the heartbeat. The task also stops
/// itself on a permanent extend failure (see
/// [`is_permanent_extend_failure`]).
pub(crate) fn start_heartbeat(
    client: CyclesClient,
    reservation_id: ReservationId,
    requested_ttl_ms: u64,
    initial_expires_at_ms: Option<u64>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Monotonic anchor for elapsed_ms. Taken when the heartbeat starts,
        // i.e. shortly AFTER the server stamped initial_expires_at_ms — so
        // elapsed_ms over-counts server elapsed time and lead_min stays a
        // lower bound.
        let anchor = tokio::time::Instant::now();
        // Cadence used after a failure before any grant sample exists; the
        // FIRST beat itself is immediate (next_beat == anchor).
        let mut delay = Duration::from_millis(held_cadence_ms(requested_ttl_ms));
        // The upcoming beat's *intended* instant; lead math measures elapsed
        // to this, keeping it an exact sum of the scheduled delays.
        let mut next_beat = anchor;

        // Grant ledger: previous known server-frame expiry, sum of observed
        // grants, and the most recent grant (None until the first success).
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

            let elapsed_ms = next_beat.duration_since(anchor).as_millis();
            if should_skip(lead_min_ms(grants_sum_ms, elapsed_ms), last_grant_ms) {
                next_beat = advance(next_beat, delay);
                continue;
            }

            let key = pending_key.clone().unwrap_or_else(IdempotencyKey::random);
            let req = ExtendRequest {
                idempotency_key: key.clone(),
                extend_by_ms: requested_ttl_ms,
                metadata: None,
            };
            match client.extend_reservation(&reservation_id, &req).await {
                Ok(resp) => {
                    pending_key = None;
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
                    // never see negative amounts.
                    let counted = u64::try_from(grant.max(0)).unwrap_or(u64::MAX);
                    grants_sum_ms += i128::from(counted);
                    last_grant_ms = Some(counted);
                    if is_lead_clamp_grant(grant, requested_ttl_ms, elapsed_since_success) {
                        // The observed "grant" tracks elapsed time, not the
                        // requested lease: pacing by it would collapse the
                        // cadence to the floor and burn max_extensions in
                        // seconds. Hold at min(requested/2, 30 s) instead.
                        delay = Duration::from_millis(held_cadence_ms(requested_ttl_ms));
                        if !warned_lead_clamp {
                            warned_lead_clamp = true;
                            tracing::warn!(
                                reservation_id = %reservation_id,
                                grant_ms = %grant,
                                elapsed_ms = %elapsed_since_success,
                                requested_ttl_ms,
                                held_cadence_ms = held_cadence_ms(requested_ttl_ms),
                                "extend grants track elapsed time, not the requested \
                                 lease — server appears to clamp the reservation's \
                                 maximum lead; holding heartbeat cadence to avoid \
                                 depleting the extension allowance"
                            );
                        }
                    } else {
                        delay =
                            Duration::from_millis(next_beat_delay_ms(counted, requested_ttl_ms));
                    }
                    // Any 2xx means the server applied the extension;
                    // `expires_at_ms` (required by the spec response schema)
                    // is authoritative regardless of the status string.
                    if resp.status != ExtendStatus::Active {
                        tracing::warn!(
                            reservation_id = %reservation_id,
                            status = ?resp.status,
                            "heartbeat extend returned 2xx with unrecognized status; \
                             treating as applied (expires_at_ms is authoritative)"
                        );
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
                Err(e) => {
                    // Keep the key (the retry must dedupe against a possibly
                    // applied-but-lost extension) and the current delay.
                    pending_key = Some(key);
                    tracing::warn!(
                        reservation_id = %reservation_id,
                        error = %e,
                        "heartbeat extend failed; retrying next beat with the same idempotency key"
                    );
                }
            }
            next_beat = advance(next_beat, delay);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUESTED: u64 = 2_000;

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
