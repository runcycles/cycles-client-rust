//! Inline commit retry with exponential backoff.

use std::time::Duration;

use crate::client::CyclesClient;
use crate::config::CyclesConfig;
use crate::error::Error;
use crate::models::request::EventCreateRequest;
use crate::models::response::{CommitResponse, EventCreateResponse};
use crate::models::{CommitRequest, ReservationId};

/// Retries failed commit operations with exponential backoff.
///
/// Wired into [`ReservationGuard::commit`](crate::guard::ReservationGuard::commit):
/// when the initial commit attempt fails with a retryable error, the guard
/// awaits [`retry`](Self::retry) inline until the commit succeeds, fails with
/// a non-retryable error, or attempts are exhausted. Retries reuse the
/// original [`CommitRequest`] — same idempotency key — so a commit that landed
/// server-side but failed on the response path cannot double-charge. Running
/// inline (not in a detached task) keeps `commit()`'s contract exact: once it
/// returns `Err`, no further commit attempt will ever be made.
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

    /// Retry a failed commit until success, a non-retryable error, or attempt
    /// exhaustion, and return the final outcome.
    ///
    /// `first_error` is the error from the initial attempt; it is returned
    /// unchanged when retry is disabled. On exhaustion the most recent error
    /// is returned.
    pub async fn retry(
        &self,
        client: &CyclesClient,
        reservation_id: &ReservationId,
        commit_request: &CommitRequest,
        first_error: Error,
    ) -> Result<CommitResponse, Error> {
        if !self.enabled {
            return Err(first_error);
        }

        tracing::debug!(
            reservation_id = %reservation_id,
            max_attempts = self.max_attempts,
            error = %first_error,
            "commit failed with retryable error; retrying with backoff"
        );

        let mut last_error = first_error;
        for attempt in 0..self.max_attempts {
            tokio::time::sleep(self.delay_for(attempt, &last_error)).await;

            match client
                .commit_reservation(reservation_id, commit_request)
                .await
            {
                Ok(resp) => {
                    tracing::debug!(
                        reservation_id = %reservation_id,
                        attempt = attempt + 1,
                        "commit retry succeeded"
                    );
                    return Ok(resp);
                }
                Err(e) if !e.is_retryable() => {
                    tracing::warn!(
                        reservation_id = %reservation_id,
                        attempt = attempt + 1,
                        error = %e,
                        "commit retry hit non-retryable error, stopping"
                    );
                    return Err(e);
                }
                Err(e) => {
                    tracing::debug!(
                        reservation_id = %reservation_id,
                        attempt = attempt + 1,
                        error = %e,
                        "commit retry attempt failed, will retry"
                    );
                    last_error = e;
                }
            }
        }

        tracing::warn!(
            reservation_id = %reservation_id,
            attempts = self.max_attempts,
            "commit retry exhausted"
        );
        Err(last_error)
    }

    /// Retry a failed direct-debit event (`POST /v1/events`) with the same
    /// bounded backoff policy as commits.
    ///
    /// Used by the commit-expired event fallback in
    /// [`ReservationGuard::commit`](crate::guard::ReservationGuard::commit):
    /// transport failures and 5xx responses on the fallback event are retried
    /// so a transient network blip doesn't turn a recoverable expired commit
    /// into unrecorded spend. Retries reuse the original
    /// [`EventCreateRequest`] — same idempotency key — so an event that
    /// already landed server-side cannot double-charge.
    pub async fn retry_event(
        &self,
        client: &CyclesClient,
        event_request: &EventCreateRequest,
        first_error: Error,
    ) -> Result<EventCreateResponse, Error> {
        if !self.enabled {
            return Err(first_error);
        }

        tracing::debug!(
            idempotency_key = %event_request.idempotency_key,
            max_attempts = self.max_attempts,
            error = %first_error,
            "fallback event failed with retryable error; retrying with backoff"
        );

        let mut last_error = first_error;
        for attempt in 0..self.max_attempts {
            tokio::time::sleep(self.delay_for(attempt, &last_error)).await;

            match client.create_event(event_request).await {
                Ok(resp) => {
                    tracing::debug!(
                        idempotency_key = %event_request.idempotency_key,
                        attempt = attempt + 1,
                        "fallback event retry succeeded"
                    );
                    return Ok(resp);
                }
                Err(e) if !e.is_retryable() => {
                    tracing::warn!(
                        idempotency_key = %event_request.idempotency_key,
                        attempt = attempt + 1,
                        error = %e,
                        "fallback event retry hit non-retryable error, stopping"
                    );
                    return Err(e);
                }
                Err(e) => {
                    tracing::debug!(
                        idempotency_key = %event_request.idempotency_key,
                        attempt = attempt + 1,
                        error = %e,
                        "fallback event retry attempt failed, will retry"
                    );
                    last_error = e;
                }
            }
        }

        tracing::warn!(
            idempotency_key = %event_request.idempotency_key,
            attempts = self.max_attempts,
            "fallback event retry exhausted"
        );
        Err(last_error)
    }

    /// Delay before the attempt that follows `last_error`.
    ///
    /// Base exponential backoff, raised to the server's `Retry-After` when
    /// the previous response carried one (HTTP 429 `LIMIT_EXCEEDED`, runtime
    /// spec v0.1.25.12): the client must wait **at least** the advertised
    /// delay, even when that exceeds `retry_max_delay`. Each error's
    /// `Retry-After` is consumed exactly once — it governs only the sleep
    /// immediately following the response that carried it.
    fn delay_for(&self, attempt: u32, last_error: &Error) -> Duration {
        let backoff = self.backoff_delay(attempt);
        match last_error.retry_after() {
            Some(retry_after) => backoff.max(retry_after),
            None => backoff,
        }
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
    use crate::models::ErrorCode;

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
            retry_initial_delay: Duration::from_millis(1),
            retry_multiplier: 2.0,
            retry_max_delay: Duration::from_secs(5),
        }
    }

    fn transient_error() -> Error {
        Error::Api {
            status: 500,
            code: Some(ErrorCode::InternalError),
            message: "boom".into(),
            request_id: None,
            retry_after: None,
            details: None,
        }
    }

    fn commit_request() -> CommitRequest {
        CommitRequest::builder()
            .actual(crate::models::Amount::usd_microcents(100))
            .build()
    }

    #[test]
    fn new_from_config() {
        let config = test_config();
        let engine = CommitRetryEngine::new(&config);
        assert!(engine.enabled);
        assert_eq!(engine.max_attempts, 3);
        assert_eq!(engine.initial_delay, Duration::from_millis(1));
        assert_eq!(engine.multiplier, 2.0);
        assert_eq!(engine.max_delay, Duration::from_secs(5));
    }

    #[test]
    fn backoff_delay_exponential() {
        let mut config = test_config();
        config.retry_initial_delay = Duration::from_millis(100);
        let engine = CommitRetryEngine::new(&config);
        // initial=100ms, multiplier=2.0
        assert_eq!(engine.backoff_delay(0), Duration::from_millis(100));
        assert_eq!(engine.backoff_delay(1), Duration::from_millis(200));
        assert_eq!(engine.backoff_delay(2), Duration::from_millis(400));
        assert_eq!(engine.backoff_delay(3), Duration::from_millis(800));
    }

    #[test]
    fn backoff_delay_capped() {
        let mut config = test_config();
        config.retry_initial_delay = Duration::from_millis(100);
        config.retry_max_delay = Duration::from_millis(300);
        let engine = CommitRetryEngine::new(&config);

        assert_eq!(engine.backoff_delay(0), Duration::from_millis(100));
        assert_eq!(engine.backoff_delay(1), Duration::from_millis(200));
        // 400ms would exceed max_delay of 300ms
        assert_eq!(engine.backoff_delay(2), Duration::from_millis(300));
        assert_eq!(engine.backoff_delay(10), Duration::from_millis(300));
    }

    fn rate_limited_error(retry_after: Duration) -> Error {
        Error::Api {
            status: 429,
            code: Some(ErrorCode::LimitExceeded),
            message: "rate limited".into(),
            request_id: None,
            retry_after: Some(retry_after),
            details: None,
        }
    }

    #[test]
    fn delay_for_waits_at_least_the_servers_retry_after() {
        let engine = CommitRetryEngine::new(&test_config());
        // Retry-After (3s) exceeds even retry_max_delay (5s cap is above,
        // but backoff at attempt 0 is 1ms) — the server delay wins.
        let err = rate_limited_error(Duration::from_secs(3));
        assert_eq!(engine.delay_for(0, &err), Duration::from_secs(3));
    }

    #[test]
    fn delay_for_exceeds_max_delay_when_retry_after_demands_it() {
        let mut config = test_config();
        config.retry_max_delay = Duration::from_millis(300);
        let engine = CommitRetryEngine::new(&config);
        let err = rate_limited_error(Duration::from_secs(10));
        // The client must wait at least Retry-After, even past the cap.
        assert_eq!(engine.delay_for(5, &err), Duration::from_secs(10));
    }

    #[test]
    fn delay_for_keeps_backoff_when_it_already_exceeds_retry_after() {
        let mut config = test_config();
        config.retry_initial_delay = Duration::from_millis(500);
        let engine = CommitRetryEngine::new(&config);
        let err = rate_limited_error(Duration::from_millis(100));
        assert_eq!(engine.delay_for(0, &err), Duration::from_millis(500));
    }

    #[test]
    fn delay_for_uses_plain_backoff_without_retry_after() {
        let mut config = test_config();
        config.retry_initial_delay = Duration::from_millis(100);
        let engine = CommitRetryEngine::new(&config);
        assert_eq!(
            engine.delay_for(1, &transient_error()),
            Duration::from_millis(200)
        );
    }

    #[tokio::test]
    async fn disabled_returns_first_error_without_attempting() {
        let mut config = test_config();
        config.retry_enabled = false;
        let engine = CommitRetryEngine::new(&config);

        // Client points at a closed port: any network attempt would fail
        // with a transport error, not the Api error we pass in.
        let client = CyclesClient::builder("key", "http://127.0.0.1:1").build();
        let id = ReservationId::new("rsv_off");

        let err = engine
            .retry(&client, &id, &commit_request(), transient_error())
            .await
            .unwrap_err();
        match err {
            Error::Api {
                status, message, ..
            } => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected the original Api error back, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retry_succeeds_and_returns_response() {
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = CyclesClient::builder("key", server.uri()).build();

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
        let resp = engine
            .retry(&client, &id, &commit_request(), transient_error())
            .await
            .unwrap();
        assert_eq!(resp.status, crate::models::CommitStatus::Committed);
    }

    #[tokio::test]
    async fn retry_stops_on_non_retryable_and_returns_it() {
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = CyclesClient::builder("key", server.uri()).build();

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

        let engine = CommitRetryEngine::new(&test_config());
        let id = ReservationId::new("rsv_nr");
        let err = engine
            .retry(&client, &id, &commit_request(), transient_error())
            .await
            .unwrap_err();
        assert!(!err.is_retryable());
    }

    fn event_request() -> EventCreateRequest {
        EventCreateRequest::builder()
            .subject(crate::models::Subject {
                tenant: Some("acme".into()),
                ..Default::default()
            })
            .action(crate::models::Action::new("llm.completion", "gpt-4o"))
            .actual(crate::models::Amount::usd_microcents(100))
            .build()
    }

    #[tokio::test]
    async fn event_retry_disabled_returns_first_error_without_attempting() {
        let mut config = test_config();
        config.retry_enabled = false;
        let engine = CommitRetryEngine::new(&config);

        // Closed port: any network attempt would surface as Transport,
        // not the seed Api error we pass in.
        let client = CyclesClient::builder("key", "http://127.0.0.1:1").build();

        let err = engine
            .retry_event(&client, &event_request(), transient_error())
            .await
            .unwrap_err();
        match err {
            Error::Api {
                status, message, ..
            } => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected the original Api error back, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn event_retry_succeeds_and_returns_response() {
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = CyclesClient::builder("key", server.uri()).build();

        Mock::given(method("POST"))
            .and(path("/v1/events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "APPLIED",
                "event_id": "evt_ok"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let engine = CommitRetryEngine::new(&test_config());
        let resp = engine
            .retry_event(&client, &event_request(), transient_error())
            .await
            .unwrap();
        assert_eq!(resp.status, crate::models::EventStatus::Applied);
        assert_eq!(resp.event_id.as_str(), "evt_ok");
    }

    #[tokio::test]
    async fn event_retry_stops_on_non_retryable_and_returns_it() {
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = CyclesClient::builder("key", server.uri()).build();

        Mock::given(method("POST"))
            .and(path("/v1/events"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "INVALID_REQUEST",
                "message": "Bad event",
                "request_id": "req-evt-1"
            })))
            .expect(1) // Only called once, not retried
            .mount(&server)
            .await;

        let engine = CommitRetryEngine::new(&test_config());
        let err = engine
            .retry_event(&client, &event_request(), transient_error())
            .await
            .unwrap_err();
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn event_retry_exhausts_attempts_and_returns_last_error() {
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = CyclesClient::builder("key", server.uri()).build();

        Mock::given(method("POST"))
            .and(path("/v1/events"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                "error": "INTERNAL_ERROR",
                "message": "Server error",
                "request_id": "req-evt-2"
            })))
            .expect(2) // max_attempts = 2
            .mount(&server)
            .await;

        let mut config = test_config();
        config.retry_max_attempts = 2;
        let engine = CommitRetryEngine::new(&config);
        let err = engine
            .retry_event(&client, &event_request(), transient_error())
            .await
            .unwrap_err();
        assert!(err.is_retryable());
        assert_eq!(err.request_id(), Some("req-evt-2"));
    }

    #[tokio::test]
    async fn retry_exhausts_attempts_and_returns_last_error() {
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let client = CyclesClient::builder("key", server.uri()).build();

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
        let engine = CommitRetryEngine::new(&config);
        let id = ReservationId::new("rsv_ex");
        let err = engine
            .retry(&client, &id, &commit_request(), transient_error())
            .await
            .unwrap_err();
        // The last server error, not the seed error, is surfaced.
        assert!(err.is_retryable());
        assert_eq!(err.request_id(), Some("req-2"));
    }
}
