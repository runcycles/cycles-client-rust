//! Error types for the Cycles client.

use std::time::Duration;

use crate::models::ErrorCode;

/// The error type for all Cycles client operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// HTTP transport error (network failure, timeout, DNS, etc.).
    #[error("HTTP transport error: {0}")]
    Transport(#[source] reqwest::Error),

    /// The server returned an error response.
    #[error("API error (HTTP {status}): {message}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Parsed error code from the response body.
        code: Option<ErrorCode>,
        /// Human-readable error message.
        message: String,
        /// Server-assigned request ID.
        request_id: Option<String>,
        /// Suggested retry delay.
        retry_after: Option<Duration>,
        /// Additional error details.
        details: Option<serde_json::Value>,
    },

    /// Budget is insufficient for the requested operation (HTTP 409).
    #[error("budget exceeded: {message}")]
    BudgetExceeded {
        /// Human-readable error message.
        message: String,
        /// Scopes that are over budget.
        affected_scopes: Vec<String>,
        /// Suggested retry delay.
        retry_after: Option<Duration>,
        /// Server-assigned request ID.
        request_id: Option<String>,
        /// HTTP status of the response this error was classified from.
        /// `None` when the error was derived from a `DENY` decision on an
        /// HTTP 200 reservation response rather than an error response.
        status: Option<u16>,
    },

    /// A commit hit `RESERVATION_EXPIRED` and the event-fallback recovery
    /// (`POST /v1/events`) also failed, so the spend is **not** recorded.
    ///
    /// When a commit lands after the reservation's grace period the server
    /// has already returned the reserved budget to the pool; the client then
    /// tries to record the spend as a post-hoc direct-debit event (same
    /// idempotency key as the commit). This variant is returned only when
    /// that fallback fails too. Both underlying errors are preserved so the
    /// caller can see why the commit expired *and* why recovery failed.
    #[error(
        "commit failed: reservation {reservation_id} expired before the commit landed and the \
         event fallback also failed — spend is NOT recorded (commit error: {commit_error}; \
         event error: {event_error})"
    )]
    CommitRecoveryFailed {
        /// The reservation whose commit expired.
        reservation_id: String,
        /// The original `RESERVATION_EXPIRED` commit error.
        commit_error: Box<Error>,
        /// The error from the failed `POST /v1/events` fallback.
        event_error: Box<Error>,
    },

    /// Actual spend is durably journaled but its server-side settlement is
    /// still unresolved.
    ///
    /// The SDK will replay this record with the same idempotency key on a
    /// later client startup or explicit
    /// [`CyclesClient::flush_pending_commits`](crate::CyclesClient::flush_pending_commits).
    /// Callers MUST NOT issue compensating settlement with a different key.
    #[error(
        "commit for reservation {reservation_id} is unresolved but durably journaled for replay: \
         {last_error}"
    )]
    CommitPending {
        /// The reservation whose actual spend remains unresolved.
        reservation_id: String,
        /// The most recent commit or event-fallback error.
        last_error: Box<Error>,
    },

    /// Failed to deserialize the response body.
    #[error("failed to deserialize response: {0}")]
    Deserialization(#[source] serde_json::Error),

    /// Invalid client configuration.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// Invalid request parameters (caught before sending).
    #[error("invalid request: {0}")]
    Validation(String),
}

impl Error {
    /// Returns `true` if the error is retryable.
    ///
    /// Transport errors, server errors (5xx), and rate limiting (HTTP 429)
    /// are retryable. 429 is retryable **by status alone** — even when the
    /// body is absent or unparseable (no typed error code), the status is
    /// authoritative and any `Retry-After` header is still honored — matching
    /// the other Cycles SDKs. Budget exceeded errors are only retryable when
    /// they came from an actual 429 rate-limit response carrying a retry
    /// delay; a 409 `BUDGET_EXCEEDED` (or a `DENY` decision) is a budget
    /// fact, not a transient fault, regardless of any suggested delay.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Api { status, code, .. } => {
                if *status >= 500 || *status == 429 {
                    return true;
                }
                code.is_some_and(|c| c.is_retryable())
            }
            Self::BudgetExceeded {
                retry_after,
                status,
                ..
            } => retry_after.is_some() && *status == Some(429),
            // Final by construction: both the commit path (including its
            // inline retry) and the event fallback (including its own bounded
            // retry) have already run to completion.
            Self::CommitRecoveryFailed { .. } | Self::CommitPending { .. } => false,
            Self::Deserialization(_) | Self::Config(_) | Self::Validation(_) => false,
        }
    }

    /// Returns `true` if this is an authentication/authorization failure
    /// (HTTP 401 `UNAUTHORIZED` or HTTP 403 `FORBIDDEN`).
    ///
    /// These are deliberately **non-retryable**: retrying with the same
    /// credentials cannot succeed. Callers should treat them as
    /// configuration problems (rotate/fix the API key, check the principal's
    /// permissions) rather than transient faults.
    pub fn is_auth_error(&self) -> bool {
        matches!(
            self,
            Self::Api {
                status: 401 | 403,
                ..
            } | Self::Api {
                code: Some(ErrorCode::Unauthorized | ErrorCode::Forbidden),
                ..
            }
        )
    }

    /// Returns `true` if this is a budget exceeded error.
    pub fn is_budget_exceeded(&self) -> bool {
        matches!(self, Self::BudgetExceeded { .. })
            || matches!(
                self,
                Self::Api {
                    code: Some(ErrorCode::BudgetExceeded),
                    ..
                }
            )
    }

    /// Returns `true` if this is a tenant-closed error (`TENANT_CLOSED`).
    ///
    /// Servers return HTTP 409 `TENANT_CLOSED` on reservation
    /// create/commit/release/extend when the owning tenant's status is
    /// CLOSED (runtime spec v0.1.25.13, mirroring governance spec Rule 2).
    /// Not retryable — the tenant must be reopened administratively.
    pub fn is_tenant_closed(&self) -> bool {
        matches!(
            self,
            Self::Api {
                code: Some(ErrorCode::TenantClosed),
                ..
            }
        )
    }

    /// Returns the HTTP status code of the response this error was built
    /// from, if any.
    ///
    /// `None` for errors that did not come from an HTTP error response
    /// (transport failures, client-side validation, and `BudgetExceeded`
    /// derived from a `DENY` decision on an HTTP 200 response).
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(*status),
            Self::BudgetExceeded { status, .. } => *status,
            _ => None,
        }
    }

    /// Returns the suggested retry delay, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            Self::BudgetExceeded { retry_after, .. } => *retry_after,
            Self::CommitRecoveryFailed { event_error, .. } => event_error.retry_after(),
            Self::CommitPending { last_error, .. } => last_error.retry_after(),
            _ => None,
        }
    }

    /// Returns the server-assigned request ID, if available.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Api { request_id, .. } => request_id.as_deref(),
            Self::BudgetExceeded { request_id, .. } => request_id.as_deref(),
            _ => None,
        }
    }

    /// Returns the error code, if available.
    pub fn error_code(&self) -> Option<ErrorCode> {
        match self {
            Self::Api { code, .. } => *code,
            Self::BudgetExceeded { .. } => Some(ErrorCode::BudgetExceeded),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Self::Transport(err)
    }
}
