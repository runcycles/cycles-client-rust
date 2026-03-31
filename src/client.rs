//! Async HTTP client for the Cycles API.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::config::{CyclesClientBuilder, CyclesConfig};
use crate::constants::{
    API_KEY_HEADER, BALANCES_PATH, DECIDE_PATH, EVENTS_PATH, IDEMPOTENCY_KEY_HEADER,
    RESERVATIONS_PATH,
};
use crate::error::Error;
use crate::guard::ReservationGuard;
use crate::models::request::{
    BalanceParams, CommitRequest, DecisionRequest, EventCreateRequest, ExtendRequest,
    ListReservationsParams, ReleaseRequest, ReservationCreateRequest,
};
use crate::models::response::{
    BalanceResponse, CommitResponse, DecisionResponse, ErrorResponse, EventCreateResponse,
    ExtendResponse, ReleaseResponse, ReservationCreateResponse, ReservationDetail,
    ReservationListResponse,
};
use crate::models::{ErrorCode, ReservationId};
use crate::response::ApiResponse;
use crate::validation;

/// Async client for the Cycles budget authority API.
///
/// The client is cheaply cloneable (uses `Arc` internally) and can be shared
/// across tasks. It is `Send + Sync`.
///
/// # Example
///
/// ```rust,no_run
/// use runcycles::{CyclesClient, models::*};
///
/// # async fn example() -> Result<(), runcycles::Error> {
/// let client = CyclesClient::builder("my-api-key", "http://localhost:7878")
///     .tenant("acme")
///     .build();
///
/// let guard = client.reserve(
///     ReservationCreateRequest::builder()
///         .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
///         .action(Action::new("llm.completion", "gpt-4o"))
///         .estimate(Amount::usd_microcents(5000))
///         .build()
/// ).await?;
///
/// // ... do work ...
///
/// guard.commit(
///     CommitRequest::builder()
///         .actual(Amount::usd_microcents(3200))
///         .build()
/// ).await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct CyclesClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    http: reqwest::Client,
    config: CyclesConfig,
}

impl std::fmt::Debug for CyclesClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CyclesClient")
            .field("base_url", &self.inner.config.base_url)
            .finish()
    }
}

impl CyclesClient {
    /// Create a new client builder.
    pub fn builder(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> CyclesClientBuilder {
        CyclesClientBuilder::new(api_key, base_url)
    }

    /// Create a client from a pre-built config.
    pub fn new(config: CyclesConfig) -> Self {
        Self::from_builder(config, None)
    }

    /// Internal constructor used by the builder.
    pub(crate) fn from_builder(
        config: CyclesConfig,
        http_client: Option<reqwest::Client>,
    ) -> Self {
        let http = http_client.unwrap_or_else(|| {
            reqwest::Client::builder()
                .connect_timeout(config.connect_timeout)
                .timeout(config.connect_timeout + config.read_timeout)
                .build()
                .expect("failed to build HTTP client")
        });

        Self {
            inner: Arc::new(ClientInner { http, config }),
        }
    }

    /// Access the client configuration.
    pub fn config(&self) -> &CyclesConfig {
        &self.inner.config
    }

    // ─── High-Level API ──────────────────────────────────────────────

    /// Reserve budget and return an RAII guard.
    ///
    /// The guard must be committed or released. If dropped without either,
    /// a best-effort release is attempted.
    ///
    /// Returns `Err(Error::BudgetExceeded)` if the decision is `Deny`.
    #[tracing::instrument(skip(self, req), fields(cycles.reservation_id, cycles.decision))]
    pub async fn reserve(
        &self,
        req: ReservationCreateRequest,
    ) -> Result<ReservationGuard, Error> {
        validation::validate_subject(&req.subject)?;
        validation::validate_ttl_ms(req.ttl_ms)?;
        validation::validate_grace_period_ms(req.grace_period_ms)?;
        validation::validate_non_negative(req.estimate.amount, "estimate.amount")?;

        let resp = self.create_reservation(&req).await?;

        if resp.decision.is_denied() {
            return Err(Error::BudgetExceeded {
                message: resp
                    .reason_code
                    .clone()
                    .unwrap_or_else(|| "budget exceeded".to_string()),
                affected_scopes: resp.affected_scopes.clone(),
                retry_after: resp.retry_after_ms.map(Duration::from_millis),
                request_id: None,
            });
        }

        let reservation_id = resp
            .reservation_id
            .clone()
            .expect("reservation_id must be present when decision is ALLOW");

        let span = tracing::Span::current();
        span.record("cycles.reservation_id", reservation_id.as_str());
        span.record("cycles.decision", tracing::field::debug(&resp.decision));

        Ok(ReservationGuard::new(
            self.clone(),
            reservation_id,
            resp.decision,
            resp.caps.clone(),
            resp.expires_at_ms,
            resp.affected_scopes.clone(),
            req.ttl_ms,
        ))
    }

    // ─── Low-Level API ──────────────────────────────────────────────

    /// Create a budget reservation.
    pub async fn create_reservation(
        &self,
        req: &ReservationCreateRequest,
    ) -> Result<ReservationCreateResponse, Error> {
        self.post_json(
            RESERVATIONS_PATH,
            req,
            Some(req.idempotency_key.as_str()),
        )
        .await
    }

    /// Create a reservation and return the response with metadata.
    pub async fn create_reservation_with_metadata(
        &self,
        req: &ReservationCreateRequest,
    ) -> Result<ApiResponse<ReservationCreateResponse>, Error> {
        self.post_json_with_metadata(
            RESERVATIONS_PATH,
            req,
            Some(req.idempotency_key.as_str()),
        )
        .await
    }

    /// Commit actual spend against a reservation.
    pub async fn commit_reservation(
        &self,
        id: &ReservationId,
        req: &CommitRequest,
    ) -> Result<CommitResponse, Error> {
        let path = format!("{RESERVATIONS_PATH}/{}/commit", id.as_str());
        self.post_json(&path, req, Some(req.idempotency_key.as_str()))
            .await
    }

    /// Release (cancel) a reservation, returning reserved budget.
    pub async fn release_reservation(
        &self,
        id: &ReservationId,
        req: &ReleaseRequest,
    ) -> Result<ReleaseResponse, Error> {
        let path = format!("{RESERVATIONS_PATH}/{}/release", id.as_str());
        self.post_json(&path, req, Some(req.idempotency_key.as_str()))
            .await
    }

    /// Extend a reservation's TTL (heartbeat).
    pub async fn extend_reservation(
        &self,
        id: &ReservationId,
        req: &ExtendRequest,
    ) -> Result<ExtendResponse, Error> {
        let path = format!("{RESERVATIONS_PATH}/{}/extend", id.as_str());
        self.post_json(&path, req, Some(req.idempotency_key.as_str()))
            .await
    }

    /// Preflight budget decision check (no reservation created).
    pub async fn decide(&self, req: &DecisionRequest) -> Result<DecisionResponse, Error> {
        self.post_json(DECIDE_PATH, req, Some(req.idempotency_key.as_str()))
            .await
    }

    /// Create a direct-debit event (no prior reservation).
    pub async fn create_event(
        &self,
        req: &EventCreateRequest,
    ) -> Result<EventCreateResponse, Error> {
        self.post_json(EVENTS_PATH, req, Some(req.idempotency_key.as_str()))
            .await
    }

    /// List reservations with optional filters.
    pub async fn list_reservations(
        &self,
        params: &ListReservationsParams,
    ) -> Result<ReservationListResponse, Error> {
        self.get_json(RESERVATIONS_PATH, Some(params)).await
    }

    /// Get details of a single reservation.
    pub async fn get_reservation(
        &self,
        id: &ReservationId,
    ) -> Result<ReservationDetail, Error> {
        let path = format!("{RESERVATIONS_PATH}/{}", id.as_str());
        self.get_json::<(), _>(&path, None).await
    }

    /// Query budget balances for scopes.
    pub async fn get_balances(
        &self,
        params: &BalanceParams,
    ) -> Result<BalanceResponse, Error> {
        if !params.has_filter() {
            return Err(Error::Validation(
                "getBalances requires at least one subject filter".to_string(),
            ));
        }
        self.get_json(BALANCES_PATH, Some(params)).await
    }

    // ─── Internal HTTP Methods ──────────────────────────────────────

    async fn post_json<B: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        idempotency_key: Option<&str>,
    ) -> Result<R, Error> {
        let resp: ApiResponse<R> = self.post_json_with_metadata(path, body, idempotency_key).await?;
        Ok(resp.into_inner())
    }

    async fn post_json_with_metadata<B: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        idempotency_key: Option<&str>,
    ) -> Result<ApiResponse<R>, Error> {
        let url = format!("{}{}", self.inner.config.base_url, path);

        let mut headers = HeaderMap::new();
        headers.insert(
            API_KEY_HEADER,
            HeaderValue::from_str(&self.inner.config.api_key)
                .map_err(|e| Error::Config(format!("invalid API key header value: {e}")))?,
        );
        if let Some(key) = idempotency_key {
            if let Ok(val) = HeaderValue::from_str(key) {
                headers.insert(IDEMPOTENCY_KEY_HEADER, val);
            }
        }

        let resp = self
            .inner
            .http
            .post(&url)
            .headers(headers)
            .json(body)
            .send()
            .await?;

        let response_headers = resp.headers().clone();
        let status = resp.status().as_u16();

        if (200..300).contains(&status) {
            let data: R = resp.json().await.map_err(|e| {
                Error::Deserialization(serde::de::Error::custom(e.to_string()))
            })?;
            Ok(ApiResponse::from_response(data, &response_headers))
        } else {
            Err(self.parse_error_response(status, resp, &response_headers).await)
        }
    }

    async fn get_json<Q: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        query: Option<&Q>,
    ) -> Result<R, Error> {
        let url = format!("{}{}", self.inner.config.base_url, path);

        let mut request = self
            .inner
            .http
            .get(&url)
            .header(API_KEY_HEADER, &self.inner.config.api_key);

        if let Some(q) = query {
            request = request.query(q);
        }

        let resp = request.send().await?;
        let response_headers = resp.headers().clone();
        let status = resp.status().as_u16();

        if (200..300).contains(&status) {
            resp.json().await.map_err(|e| {
                Error::Deserialization(serde::de::Error::custom(e.to_string()))
            })
        } else {
            Err(self.parse_error_response(status, resp, &response_headers).await)
        }
    }

    async fn parse_error_response(
        &self,
        status: u16,
        resp: reqwest::Response,
        headers: &HeaderMap,
    ) -> Error {
        let request_id = headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let body: Option<ErrorResponse> = resp.json().await.ok();

        let message = body
            .as_ref()
            .map(|b| b.message.clone())
            .unwrap_or_else(|| format!("HTTP {status}"));

        let error_code: Option<ErrorCode> = body
            .as_ref()
            .and_then(|b| serde_json::from_value(serde_json::Value::String(b.error.clone())).ok());

        let details = body
            .as_ref()
            .and_then(|b| b.details.clone());

        // Classify specific error types
        if status == 409 {
            if let Some(ErrorCode::BudgetExceeded) = error_code {
                return Error::BudgetExceeded {
                    message,
                    affected_scopes: vec![],
                    retry_after: None,
                    request_id,
                };
            }
        }

        Error::Api {
            status,
            code: error_code,
            message,
            request_id,
            retry_after: None,
            details,
        }
    }
}

// Ensure CyclesClient is Send + Sync.
#[cfg(test)]
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    fn check() {
        assert_send_sync::<CyclesClient>();
    }
};
