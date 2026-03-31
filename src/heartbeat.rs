//! Background TTL extension (heartbeat) for active reservations.

use tokio_util::sync::CancellationToken;

use crate::client::CyclesClient;
use crate::models::{ExtendRequest, ReservationId};

/// Spawn a background task that periodically extends a reservation's TTL.
///
/// The task fires at `ttl_ms / 2` intervals (minimum 1 second) and calls
/// `extend_reservation` to keep the reservation alive.
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
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(interval) => {
                    let req = ExtendRequest::new(ttl_ms);
                    let result = client.extend_reservation(&reservation_id, &req).await;
                    if let Err(e) = result {
                        tracing::warn!(
                            reservation_id = %reservation_id,
                            error = %e,
                            "heartbeat extend failed"
                        );
                    }
                }
            }
        }
    })
}
