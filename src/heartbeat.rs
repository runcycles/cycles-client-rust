//! Background TTL extension (heartbeat) for active reservations.
//!
//! # Grant-ledger design (v2.2)
//!
//! The protocol's `extend_by_ms` is relative to the reservation's **current
//! `expires_at_ms`**, not to request time (`cycles-protocol-v0.yaml`), so the
//! heartbeat must decide per beat whether an extension is actually needed —
//! blindly extending every beat drifts the expiry outward, and fixed
//! skip-every-other-beat cadences lapse for small TTLs or clamped grants.
//!
//! ## Why the HTTP `Date` header is only a hint
//!
//! An earlier revision derived an "effective TTL" from the reserve
//! response's `expires_at_ms` minus its HTTP `Date` header and let it drive
//! the whole scheduler. Spec review (round 3) rejected that as a correctness
//! input: RFC 9110's `Date` is a whole-second, best-effort *origination*
//! timestamp that intermediaries may replace, and in cycles-server
//! `expires_at_ms` is stamped from Redis `TIME` while `Date` comes from the
//! HTTP layer — they are **not the same clock**, so their difference is not
//! a trustworthy lease measurement. Worse, clamping the difference upward
//! (the old 1000 ms floor) *fabricated* lease the server never granted. The
//! `Date`-derived estimate survives only as a **first-beat cadence hint**
//! ([`date_ttl_hint_ms`] / [`first_beat_delay_ms`]): it can pull the first
//! beat earlier when the server appears to have capped the grant, but no
//! correctness property depends on it, and it is never clamped upward.
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
//! ## Cadence
//!
//! Beat delays are computed per beat, not from a fixed interval:
//!
//! - **first beat**: `min(requested_ttl/2, 30 s, date_hint/2)` (the hint arm
//!   only when derivable and > 0) — the 30 s cap bounds the damage when a
//!   silently-capped grant made the requested TTL a wild overestimate;
//! - **after a success**: `clamp(last_grant/2, 500 ms, requested_ttl/2)` —
//!   the cadence tracks what the server actually grants (clamped grants →
//!   faster beats), floored at 500 ms so tiny/zero grants cannot busy-loop;
//! - **after a transient failure**: unchanged — retry at the current cadence
//!   with the **same idempotency key** (the server replays the original
//!   outcome, so an applied-but-lost extension cannot double-extend).
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

/// Hard cap on the first-beat delay. Even a huge requested TTL gets its
/// first beat within 30 s, bounding how late the heartbeat discovers a
/// silently-capped grant when no usable `Date` hint exists.
const FIRST_BEAT_DELAY_CAP_MS: u64 = 30_000;

/// Floor on post-success beat delays: even zero or tiny grants retry at
/// 500 ms, never a busy loop.
const MIN_BEAT_DELAY_MS: u64 = 500;

/// The `Date`-derived TTL estimate: `expires_at_ms − date_ms`, both from the
/// reserve response, **raw** (saturating at 0) — no clamping in either
/// direction, because this is a cadence *hint*, not a lease measurement (see
/// module docs: `Date` is not the same clock as `expires_at_ms`, and an
/// upward clamp would fabricate lease). `None` when either sample is
/// missing.
pub(crate) fn date_ttl_hint_ms(expires_at_ms: Option<u64>, date_ms: Option<u64>) -> Option<u64> {
    match (expires_at_ms, date_ms) {
        (Some(expires), Some(date)) => Some(expires.saturating_sub(date)),
        _ => None,
    }
}

/// Delay before the first beat: `min(requested/2, 30 s cap, hint/2)`, the
/// hint arm only when derivable and non-zero. `.max(1)` keeps the delay
/// non-zero even for a degenerate (sub-2 ms) hint.
pub(crate) fn first_beat_delay_ms(requested_ttl_ms: u64, date_ttl_hint_ms: Option<u64>) -> u64 {
    let mut delay = (requested_ttl_ms / 2).min(FIRST_BEAT_DELAY_CAP_MS);
    if let Some(hint) = date_ttl_hint_ms.filter(|&h| h > 0) {
        delay = delay.min(hint / 2);
    }
    delay.max(1)
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
/// `date_ttl_hint_ms` is the raw `Date`-derived TTL estimate from
/// [`date_ttl_hint_ms`] — a first-beat cadence hint only.
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
    date_ttl_hint_ms: Option<u64>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Monotonic anchor for elapsed_ms. Taken when the heartbeat starts,
        // i.e. shortly AFTER the server stamped initial_expires_at_ms — so
        // elapsed_ms over-counts server elapsed time and lead_min stays a
        // lower bound.
        let anchor = tokio::time::Instant::now();
        let mut delay =
            Duration::from_millis(first_beat_delay_ms(requested_ttl_ms, date_ttl_hint_ms));
        // The upcoming beat's *intended* instant; lead math measures elapsed
        // to this, keeping it an exact sum of the scheduled delays.
        let mut next_beat = anchor + delay;

        // Grant ledger: previous known server-frame expiry, sum of observed
        // grants, and the most recent grant (None until the first success).
        let mut prev_expiry = initial_expires_at_ms;
        let mut grants_sum_ms: i128 = 0;
        let mut last_grant_ms: Option<u64> = None;
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
                    // expiries. `ExtendResponse::expires_at_ms` is
                    // structurally required, so the only possible missing
                    // sample is a reserve response without expires_at_ms
                    // (non-conformant server): fall back to the requested
                    // amount. saturating_sub: an expiry that moved backwards
                    // counts as a zero grant (conservative).
                    let grant = match prev_expiry {
                        Some(prev) => resp.expires_at_ms.saturating_sub(prev),
                        None => requested_ttl_ms,
                    };
                    prev_expiry = Some(resp.expires_at_ms);
                    grants_sum_ms += i128::from(grant);
                    last_grant_ms = Some(grant);
                    delay = Duration::from_millis(next_beat_delay_ms(grant, requested_ttl_ms));
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
    fn full_grant_trace_extends_four_beats_then_skips() {
        // grants == requested, 1000 ms beats: extend@1..4, skip@5 (bound
        // exactly 1.5·grant), extend@6 — the v2.2 steady state.
        let mut grants: i128 = 0;
        let mut extends = Vec::new();
        for beat in 1u128..=6 {
            let lead = lead_min_ms(grants, beat * 1_000);
            let skip = should_skip(lead, if beat == 1 { None } else { Some(REQUESTED) });
            extends.push(!skip);
            if !skip {
                grants += i128::from(REQUESTED);
            }
        }
        assert_eq!(extends, [true, true, true, true, false, true]);
    }

    #[test]
    fn first_beat_delay_pins() {
        // requested 2000, no hint → requested/2 = 1000.
        assert_eq!(first_beat_delay_ms(2_000, None), 1_000);
        // Large requested, no hint → the 30 s cap.
        assert_eq!(first_beat_delay_ms(86_400_000, None), 30_000);
        // Capped-grant shape: requested 8000, hint 2000 → hint/2 = 1000.
        assert_eq!(first_beat_delay_ms(8_000, Some(2_000)), 1_000);
        // A hint larger than the request never delays past requested/2.
        assert_eq!(first_beat_delay_ms(2_000, Some(10_000)), 1_000);
        // A zero hint (expiry at or before Date — the raw, unclamped
        // estimate) is ignored, not treated as "beat immediately".
        assert_eq!(first_beat_delay_ms(2_000, Some(0)), 1_000);
        // Degenerate 1 ms hint: hint/2 floors to 0, kept non-zero.
        assert_eq!(first_beat_delay_ms(2_000, Some(1)), 1);
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
    fn date_hint_is_raw_and_unclamped() {
        let date = 1_700_000_000_000;
        // The old code clamped 200 up to 1000 (fabricating lease); the hint
        // is raw.
        assert_eq!(date_ttl_hint_ms(Some(date + 200), Some(date)), Some(200));
        // Expiry at or before Date saturates to 0 (callers ignore 0).
        assert_eq!(date_ttl_hint_ms(Some(date - 5_000), Some(date)), Some(0));
        // Larger than any request is fine — first_beat_delay_ms bounds it.
        assert_eq!(
            date_ttl_hint_ms(Some(date + 3_600_000), Some(date)),
            Some(3_600_000)
        );
        // Missing either sample → no hint.
        assert_eq!(date_ttl_hint_ms(None, Some(date)), None);
        assert_eq!(date_ttl_hint_ms(Some(date), None), None);
        assert_eq!(date_ttl_hint_ms(None, None), None);
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
