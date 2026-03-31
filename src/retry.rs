//! Background commit retry engine with exponential backoff.

use std::time::Duration;

use crate::client::CyclesClient;
use crate::config::CyclesConfig;
use crate::models::{CommitRequest, ReservationId};

/// Retries failed commit operations in the background with exponential backoff.
#[derive(Debug, Clone)]
pub(crate) struct CommitRetryEngine {
    enabled: bool,
    max_attempts: u32,
    initial_delay: Duration,
    multiplier: f64,
    max_delay: Duration,
}

impl CommitRetryEngine {
    /// Create a new retry engine from client configuration.
    pub fn new(config: &CyclesConfig) -> Self {
        Self {
            enabled: config.retry_enabled,
            max_attempts: config.retry_max_attempts,
            initial_delay: config.retry_initial_delay,
            multiplier: config.retry_multiplier,
            max_delay: config.retry_max_delay,
        }
    }

    /// Schedule a fire-and-forget background retry for a failed commit.
    pub fn schedule(
        &self,
        client: CyclesClient,
        reservation_id: ReservationId,
        commit_request: CommitRequest,
    ) {
        if !self.enabled {
            return;
        }

        let engine = self.clone();
        tokio::spawn(async move {
            engine
                .retry_loop(client, reservation_id, commit_request)
                .await;
        });
    }

    async fn retry_loop(
        &self,
        client: CyclesClient,
        reservation_id: ReservationId,
        commit_request: CommitRequest,
    ) {
        for attempt in 0..self.max_attempts {
            let backoff = self.backoff_delay(attempt);
            tokio::time::sleep(backoff).await;

            match client
                .commit_reservation(&reservation_id, &commit_request)
                .await
            {
                Ok(_) => return,
                Err(e) if !e.is_retryable() => {
                    tracing::warn!(
                        reservation_id = %reservation_id,
                        attempt = attempt + 1,
                        error = %e,
                        "commit retry hit non-retryable error, stopping"
                    );
                    return;
                }
                Err(e) => {
                    tracing::debug!(
                        reservation_id = %reservation_id,
                        attempt = attempt + 1,
                        error = %e,
                        "commit retry attempt failed, will retry"
                    );
                }
            }
        }

        tracing::warn!(
            reservation_id = %reservation_id,
            attempts = self.max_attempts,
            "commit retry exhausted"
        );
    }

    fn backoff_delay(&self, attempt: u32) -> Duration {
        let delay = self.initial_delay.as_millis() as f64 * self.multiplier.powi(attempt as i32);
        let capped = delay.min(self.max_delay.as_millis() as f64);
        Duration::from_millis(capped as u64)
    }
}
