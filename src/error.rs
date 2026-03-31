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
    /// Transport errors and server errors (5xx) are generally retryable.
    /// Budget exceeded errors are not retryable unless the server suggests a retry delay.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Api {
                status, code, ..
            } => {
                if *status >= 500 {
                    return true;
                }
                code.is_some_and(|c| c.is_retryable())
            }
            Self::BudgetExceeded { retry_after, .. } => retry_after.is_some(),
            Self::Deserialization(_) | Self::Config(_) | Self::Validation(_) => false,
        }
    }

    /// Returns `true` if this is a budget exceeded error.
    pub fn is_budget_exceeded(&self) -> bool {
        matches!(self, Self::BudgetExceeded { .. })
            || matches!(self, Self::Api { code: Some(ErrorCode::BudgetExceeded), .. })
    }

    /// Returns the suggested retry delay, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            Self::BudgetExceeded { retry_after, .. } => *retry_after,
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
