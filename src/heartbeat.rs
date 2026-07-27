//! Background TTL extension (heartbeat) for active reservations.

use tokio_util::sync::CancellationToken;

use crate::client::CyclesClient;
use crate::models::enums::ExtendStatus;
use crate::models::{ExtendRequest, ReservationId};

/// Spawn a background task that periodically extends a reservation's TTL.
///
/// The task ticks at `ttl_ms / 2` intervals (minimum 1 second) and extends
/// on **alternate beats**: the first beat extends (only `ttl/2` of lifetime
/// remains at that point), and each *successful* extend skips exactly one
/// beat before the next one. A failed extend (transport/API error, or a
/// response whose status is not `ACTIVE`) is retried on the very next beat.
///
/// Rationale: the protocol's `extend_by_ms` is relative to the reservation's
/// **current `expires_at_ms`**, not to request time (cycles-protocol-v0.yaml).
/// Extending by `ttl_ms` on every beat therefore drifts the expiry outward by
/// `ttl/2` per beat — a killed process would leave the reserved budget locked
/// until the drifted expiry, and the server-side `max_extensions` allowance
/// burns twice as fast as needed. Extending on alternate beats keeps the
/// expiry lead oscillating within `[ttl/2, 1.5·ttl]` with no drift.
///
/// The client clock is deliberately never compared against the server's
/// `expires_at_ms` — clock skew would make that unsafe.
///
/// Returns a `JoinHandle` that can be used to await the task. Cancel the
/// provided `CancellationToken` to stop the heartbeat.
pub(crate) fn start_heartbeat(
    client: CyclesClient,
    reservation_id: ReservationId,
    ttl_ms: u64,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let interval = std::time::Duration::from_millis((ttl_ms / 2).max(1_000));

    tokio::spawn(async move {
        // False initially so the FIRST beat extends.
        let mut skip_this_beat = false;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(interval) => {
                    if skip_this_beat {
                        // Previous beat extended successfully; the expiry is
                        // a full ttl away. Skip exactly this one beat.
                        skip_this_beat = false;
                        continue;
                    }
                    let req = ExtendRequest::new(ttl_ms);
                    match client.extend_reservation(&reservation_id, &req).await {
                        Ok(resp) if resp.status == ExtendStatus::Active => {
                            skip_this_beat = true;
                        }
                        Ok(resp) => {
                            // HTTP success with an unrecognized status is not
                            // proof the TTL was extended — retry next beat.
                            tracing::warn!(
                                reservation_id = %reservation_id,
                                status = ?resp.status,
                                "heartbeat extend returned non-ACTIVE status; retrying next beat"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                reservation_id = %reservation_id,
                                error = %e,
                                "heartbeat extend failed; retrying next beat"
                            );
                        }
                    }
                }
            }
        }
    })
}
