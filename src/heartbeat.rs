//! Background TTL extension (heartbeat) for active reservations.
//!
//! # Lead-estimate design
//!
//! The protocol's `extend_by_ms` is relative to the reservation's **current
//! `expires_at_ms`**, not to request time (`cycles-protocol-v0.yaml`), so the
//! heartbeat must decide per beat whether an extension is actually needed —
//! blindly extending every beat drifts the expiry outward, and fixed
//! skip-every-other-beat cadences lapse for small TTLs or clamped grants.
//!
//! Instead the heartbeat estimates its **lead** — how far the server-side
//! expiry is ahead of "now" — using only same-frame arithmetic:
//!
//! ```text
//! lead_ms = (known_expiry - initial_expiry) + ttl_ms - elapsed_ms
//! ```
//!
//! where `known_expiry` and `initial_expiry` are both server-frame Unix
//! milliseconds (from the reserve response and each extend response), and
//! `elapsed_ms` is client-monotonic time since the heartbeat started
//! (`tokio::time::Instant`). Server-frame minus server-frame, monotonic minus
//! monotonic: the client wall clock is **never** compared against the
//! server's `expires_at_ms`, so clock skew cannot corrupt the estimate. The
//! estimate is conservative by construction — `elapsed_ms` includes the
//! network delay of the original reserve call, so the true lead is only ever
//! *larger* than the estimate (safe direction: we extend a little early,
//! never late).
//!
//! Per beat (every `ttl/2`, no floor):
//! - `lead >= 1.5·ttl` → skip (extending now would only build drift);
//! - otherwise extend by `ttl_ms`. Any HTTP-success (2xx) response counts as
//!   **applied** — a 2xx means the server DID apply the extension and its
//!   `expires_at_ms` is authoritative proof — so `known_expiry` is updated
//!   from the response even if the status field is unrecognized (a warning is
//!   logged). Treating an odd status as failure would re-extend every beat
//!   against such a server: exactly the drift this module exists to prevent.
//! - Transient failures keep the request's idempotency key and reuse it on
//!   the next beat, so a lost response cannot double-extend when the retry
//!   lands (the server replays the original outcome for the same key).
//! - Permanent failures — `RESERVATION_EXPIRED`, `RESERVATION_FINALIZED`,
//!   `MAX_EXTENSIONS_EXCEEDED`, or any HTTP 410 — terminate the heartbeat:
//!   no further extend can ever succeed, so retrying is pure noise.
//!
//! Beats are scheduled with [`tokio::time::interval_at`], so a slow extend
//! round-trip does not push subsequent beats later (no per-beat RTT slip);
//! [`MissedTickBehavior::Skip`] realigns after long stalls instead of
//! bursting. The lead math uses the tick's *scheduled* instant, keeping
//! `elapsed_ms` an exact multiple of the interval (scheduling jitter shows up
//! as extra true lead, again the safe direction).

use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::client::CyclesClient;
use crate::error::Error;
use crate::models::enums::{ErrorCode, ExtendStatus};
use crate::models::{ExtendRequest, IdempotencyKey, ReservationId};

/// Estimated expiry lead, in ms (signed — may be negative if beats stalled).
///
/// All inputs are same-frame differences: `known_expiry - initial_expiry` is
/// server-frame ms, `elapsed_ms` is client-monotonic ms. See module docs.
fn lead_estimate_ms(known_expiry: u64, initial_expiry: u64, ttl_ms: u64, elapsed_ms: u128) -> i128 {
    (known_expiry as i128 - initial_expiry as i128) + ttl_ms as i128 - elapsed_ms as i128
}

/// A beat is skipped when the estimated lead is at least `1.5 · ttl`
/// (integer form: `2·lead >= 3·ttl`).
fn should_skip(lead_ms: i128, ttl_ms: u64) -> bool {
    2 * lead_ms >= 3 * ttl_ms as i128
}

/// `true` for extend failures that no retry can ever fix: the reservation is
/// gone (`RESERVATION_EXPIRED` / any HTTP 410), already committed or released
/// (`RESERVATION_FINALIZED`), or out of extensions
/// (`MAX_EXTENSIONS_EXCEEDED`).
fn is_permanent_extend_failure(e: &Error) -> bool {
    matches!(
        e.error_code(),
        Some(
            ErrorCode::ReservationExpired
                | ErrorCode::ReservationFinalized
                | ErrorCode::MaxExtensionsExceeded
        )
    ) || e.status() == Some(410)
}

/// Spawn a background task that periodically extends a reservation's TTL.
///
/// Ticks every `ttl_ms / 2` (no floor — the spec's minimum `ttl_ms` of 1000
/// yields a 500 ms interval; any floor would guarantee a lapse for TTLs below
/// twice the floor). Each beat estimates the expiry lead and extends only
/// when it has fallen below `1.5 · ttl`; see the module docs for the full
/// design and safety argument.
///
/// `initial_expires_at_ms` is the `expires_at_ms` from the reserve response
/// (server frame). If the server omitted it (non-conformant — the spec
/// requires it on allowed non-dry-run reservations), the lead cannot be
/// estimated and the heartbeat degrades to extending every beat: for such a
/// server we accept drift to preserve liveness.
///
/// Returns a `JoinHandle` that can be used to await the task. Cancel the
/// provided `CancellationToken` to stop the heartbeat. The task also stops
/// itself on a permanent extend failure (see
/// [`is_permanent_extend_failure`]).
pub(crate) fn start_heartbeat(
    client: CyclesClient,
    reservation_id: ReservationId,
    ttl_ms: u64,
    initial_expires_at_ms: Option<u64>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    // `.max(1)`: `reserve()` validates ttl_ms >= 1000, but a zero-period
    // interval would panic — stay defensive.
    let interval = std::time::Duration::from_millis((ttl_ms / 2).max(1));

    tokio::spawn(async move {
        // Monotonic anchor for elapsed_ms. Taken when the heartbeat starts,
        // i.e. shortly AFTER the server stamped initial_expires_at_ms — so
        // elapsed_ms overestimates and the lead estimate is conservative.
        let anchor = tokio::time::Instant::now();
        let mut ticker = tokio::time::interval_at(anchor + interval, interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let initial_expiry = initial_expires_at_ms;
        let mut known_expiry = initial_expires_at_ms;
        // Set while an extend outcome is unresolved (transient failure):
        // reused on the retry so a lost response cannot double-extend.
        let mut pending_key: Option<IdempotencyKey> = None;

        loop {
            let tick = tokio::select! {
                () = cancel.cancelled() => break,
                tick = ticker.tick() => tick,
            };

            if let (Some(known), Some(initial)) = (known_expiry, initial_expiry) {
                // Scheduled tick instant, not Instant::now(): exact interval
                // multiples, and any fire-late jitter only adds true lead.
                let elapsed_ms = tick.duration_since(anchor).as_millis();
                let lead_ms = lead_estimate_ms(known, initial, ttl_ms, elapsed_ms);
                if should_skip(lead_ms, ttl_ms) {
                    continue;
                }
            }

            let key = pending_key.clone().unwrap_or_else(IdempotencyKey::random);
            let req = ExtendRequest {
                idempotency_key: key.clone(),
                extend_by_ms: ttl_ms,
                metadata: None,
            };
            match client.extend_reservation(&reservation_id, &req).await {
                Ok(resp) => {
                    pending_key = None;
                    // Any 2xx means the server applied the extension;
                    // `expires_at_ms` (required by the spec response schema)
                    // is authoritative regardless of the status string.
                    known_expiry = Some(resp.expires_at_ms);
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
                    // Keep the key: the retry must dedupe against a possibly
                    // applied-but-lost extension.
                    pending_key = Some(key);
                    tracing::warn!(
                        reservation_id = %reservation_id,
                        error = %e,
                        "heartbeat extend failed; retrying next beat with the same idempotency key"
                    );
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: u64 = 1_000;

    #[test]
    fn lead_estimate_first_beat() {
        // At the first beat (elapsed ttl/2, no extensions yet) half the
        // lifetime remains.
        let lead = lead_estimate_ms(5_000, 5_000, TTL, 500);
        assert_eq!(lead, 500);
        assert!(!should_skip(lead, TTL));
    }

    #[test]
    fn lead_estimate_steady_state_skips_at_1_5_ttl() {
        // Two full-ttl grants by beat 3 (elapsed 1500): lead is exactly
        // 1.5·ttl — skip (inclusive threshold).
        let lead = lead_estimate_ms(7_000, 5_000, TTL, 1_500);
        assert_eq!(lead, 1_500);
        assert!(should_skip(lead, TTL));
        // One ms less lead → extend.
        assert!(!should_skip(1_499, TTL));
    }

    #[test]
    fn lead_estimate_goes_negative_when_stalled() {
        // Signed math: a long stall (elapsed far past the grants) yields a
        // negative lead rather than a wrapped huge one.
        let lead = lead_estimate_ms(6_000, 5_000, TTL, 10_000);
        assert_eq!(lead, -8_000);
        assert!(!should_skip(lead, TTL));
    }

    #[test]
    fn clamped_grants_never_skip() {
        // Server grants only ttl/4 per extend: lead keeps shrinking, so
        // every beat extends (liveness over cadence).
        let mut known = 5_000u64;
        for beat in 1u64..=4 {
            let lead = lead_estimate_ms(known, 5_000, TTL, u128::from(beat) * 500);
            assert!(
                !should_skip(lead, TTL),
                "beat {beat} must extend (lead {lead})"
            );
            known += TTL / 4;
        }
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
        // Transient shapes are not.
        assert!(!is_permanent_extend_failure(&api(
            500,
            Some(ErrorCode::InternalError)
        )));
        assert!(!is_permanent_extend_failure(&api(429, None)));
        assert!(!is_permanent_extend_failure(&Error::Validation("x".into())));
    }
}
