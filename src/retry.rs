//! Background commit retry engine with exponential backoff.

use std::time::Duration;

use crate::client::CyclesClient;
use crate::config::CyclesConfig;
use crate::models::{CommitRequest, ReservationId};

/// Retries failed commit operations in the background with exponential backoff.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Wired into guard commit-failure path in a future release
pub(crate) struct CommitRetryEngine {
    enabled: bool,
    max_attempts: u32,
    initial_delay: Duration,
    multiplier: f64,
    max_delay: Duration,
}

#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CyclesConfig {
        CyclesConfig {
            base_url: "http://localhost:7878".into(),
            api_key: "test".into(),
            tenant: None,
            workspace: None,
            app: None,
            workflow: None,
            agent: None,
            toolset: None,
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(5),
            retry_enabled: true,
            retry_max_attempts: 3,
            retry_initial_delay: Duration::from_millis(100),
            retry_multiplier: 2.0,
            retry_max_delay: Duration::from_secs(5),
        }
    }

    #[test]
    fn new_from_config() {
        let config = test_config();
        let engine = CommitRetryEngine::new(&config);
        assert!(engine.enabled);
        assert_eq!(engine.max_attempts, 3);
        assert_eq!(engine.initial_delay, Duration::from_millis(100));
        assert_eq!(engine.multiplier, 2.0);
        assert_eq!(engine.max_delay, Duration::from_secs(5));
    }

    #[test]
    fn backoff_delay_exponential() {
        let engine = CommitRetryEngine::new(&test_config());
        // initial=100ms, multiplier=2.0
        assert_eq!(engine.backoff_delay(0), Duration::from_millis(100));
        assert_eq!(engine.backoff_delay(1), Duration::from_millis(200));
        assert_eq!(engine.backoff_delay(2), Duration::from_millis(400));
        assert_eq!(engine.backoff_delay(3), Duration::from_millis(800));
    }

    #[test]
    fn backoff_delay_capped() {
        let mut config = test_config();
        config.retry_max_delay = Duration::from_millis(300);
        let engine = CommitRetryEngine::new(&config);

        assert_eq!(engine.backoff_delay(0), Duration::from_millis(100));
        assert_eq!(engine.backoff_delay(1), Duration::from_millis(200));
        // 400ms would exceed max_delay of 300ms
        assert_eq!(engine.backoff_delay(2), Duration::from_millis(300));
        assert_eq!(engine.backoff_delay(10), Duration::from_millis(300));
    }

    #[test]
    fn disabled_schedule_is_noop() {
        let mut config = test_config();
        config.retry_enabled = false;
        let engine = CommitRetryEngine::new(&config);
        assert!(!engine.enabled);

        // schedule should not panic even with a client pointing nowhere
        let client = CyclesClient::builder("key", "http://127.0.0.1:1").build();
        let id = ReservationId::new("rsv_noop");
        let req = CommitRequest::builder()
            .actual(crate::models::Amount::usd_microcents(100))
            .build();
        engine.schedule(client, id, req);
        // No panic = success
    }

    #[tokio::test]
    async fn retry_loop_succeeds_on_first_attempt() {
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = CyclesClient::builder("key", &server.uri()).build();

        Mock::given(method("POST"))
            .and(path("/v1/reservations/rsv_ok/commit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "COMMITTED",
                "charged": {"unit": "USD_MICROCENTS", "amount": 100}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let engine = CommitRetryEngine::new(&test_config());
        let id = ReservationId::new("rsv_ok");
        let req = CommitRequest::builder()
            .actual(crate::models::Amount::usd_microcents(100))
            .build();

        engine.retry_loop(client, id, req).await;
    }

    #[tokio::test]
    async fn retry_loop_stops_on_non_retryable() {
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = CyclesClient::builder("key", &server.uri()).build();

        Mock::given(method("POST"))
            .and(path("/v1/reservations/rsv_nr/commit"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "INVALID_REQUEST",
                "message": "Bad request",
                "request_id": "req-1"
            })))
            .expect(1) // Only called once, not retried
            .mount(&server)
            .await;

        let mut config = test_config();
        config.retry_initial_delay = Duration::from_millis(1);
        let engine = CommitRetryEngine::new(&config);
        let id = ReservationId::new("rsv_nr");
        let req = CommitRequest::builder()
            .actual(crate::models::Amount::usd_microcents(100))
            .build();

        engine.retry_loop(client, id, req).await;
    }

    #[tokio::test]
    async fn retry_loop_exhausts_attempts() {
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = CyclesClient::builder("key", &server.uri()).build();

        Mock::given(method("POST"))
            .and(path("/v1/reservations/rsv_ex/commit"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                "error": "INTERNAL_ERROR",
                "message": "Server error",
                "request_id": "req-2"
            })))
            .expect(2) // max_attempts = 2
            .mount(&server)
            .await;

        let mut config = test_config();
        config.retry_max_attempts = 2;
        config.retry_initial_delay = Duration::from_millis(1);
        let engine = CommitRetryEngine::new(&config);
        let id = ReservationId::new("rsv_ex");
        let req = CommitRequest::builder()
            .actual(crate::models::Amount::usd_microcents(100))
            .build();

        engine.retry_loop(client, id, req).await;
    }
}
