//! Protocol enumerations.
//!
//! All enums are `#[non_exhaustive]` for forward compatibility with future
//! protocol versions.

use serde::{Deserialize, Serialize};

/// Budget decision returned by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum Decision {
    /// Full budget available; proceed without constraints.
    Allow,
    /// Budget available with soft constraints (see [`Caps`](super::Caps)).
    AllowWithCaps,
    /// Insufficient budget; request denied.
    Deny,
}

impl Decision {
    /// Returns `true` if the decision permits the operation.
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allow | Self::AllowWithCaps)
    }

    /// Returns `true` if the decision is `Deny`.
    pub fn is_denied(self) -> bool {
        matches!(self, Self::Deny)
    }
}

/// Unit of measurement for budget amounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum Unit {
    /// US dollar microcents (10^-6 cents); exact accounting.
    UsdMicrocents,
    /// Integer token counts.
    Tokens,
    /// Generic integer credits.
    Credits,
    /// Risk-scoring points.
    RiskPoints,
}

/// Policy for handling overage when committing actual spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum CommitOveragePolicy {
    /// Reject if budget would be exceeded.
    Reject,
    /// Allow if budget is available (but not overdraft).
    AllowIfAvailable,
    /// Allow even if it creates debt (overdraft).
    AllowWithOverdraft,
}

/// Status of a reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum ReservationStatus {
    /// Reservation is active and holds budget.
    Active,
    /// Reservation has been committed (actual spend recorded).
    Committed,
    /// Reservation has been released (budget returned).
    Released,
    /// Reservation has expired (TTL elapsed).
    Expired,
}

/// Status returned after a successful commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum CommitStatus {
    /// The reservation was committed successfully.
    Committed,
}

/// Status returned after a successful release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum ReleaseStatus {
    /// The reservation was released successfully.
    Released,
}

/// Status returned after a successful TTL extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum ExtendStatus {
    /// The reservation is still active with extended TTL.
    Active,
}

/// Status returned after a successful event (direct debit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum EventStatus {
    /// The event was applied successfully.
    Applied,
}

/// Error codes returned by the Cycles server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum ErrorCode {
    /// The request was malformed or invalid.
    InvalidRequest,
    /// Authentication failed.
    Unauthorized,
    /// The authenticated principal lacks permission.
    Forbidden,
    /// The requested resource was not found.
    NotFound,
    /// Budget is insufficient for the requested operation.
    BudgetExceeded,
    /// The budget scope is frozen.
    BudgetFrozen,
    /// The budget scope is closed.
    BudgetClosed,
    /// The reservation has expired (TTL elapsed).
    ReservationExpired,
    /// The reservation has already been committed or released.
    ReservationFinalized,
    /// Idempotency key was reused with different parameters.
    IdempotencyMismatch,
    /// The unit in the commit does not match the reservation.
    UnitMismatch,
    /// The overdraft limit for the scope has been exceeded.
    OverdraftLimitExceeded,
    /// Outstanding debt prevents the operation.
    DebtOutstanding,
    /// Maximum number of TTL extensions reached.
    MaxExtensionsExceeded,
    /// An internal server error occurred.
    InternalError,
    /// An unknown error code was returned.
    Unknown,
}

impl ErrorCode {
    /// Returns `true` if the error is retryable.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::InternalError | Self::Unknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_helpers() {
        assert!(Decision::Allow.is_allowed());
        assert!(Decision::AllowWithCaps.is_allowed());
        assert!(!Decision::Deny.is_allowed());
        assert!(Decision::Deny.is_denied());
    }

    #[test]
    fn serde_roundtrip_decision() {
        let json = serde_json::to_string(&Decision::AllowWithCaps).unwrap();
        assert_eq!(json, "\"ALLOW_WITH_CAPS\"");
        let d: Decision = serde_json::from_str(&json).unwrap();
        assert_eq!(d, Decision::AllowWithCaps);
    }

    #[test]
    fn serde_roundtrip_unit() {
        let json = serde_json::to_string(&Unit::UsdMicrocents).unwrap();
        assert_eq!(json, "\"USD_MICROCENTS\"");
        let u: Unit = serde_json::from_str(&json).unwrap();
        assert_eq!(u, Unit::UsdMicrocents);
    }

    #[test]
    fn serde_roundtrip_error_code() {
        let json = serde_json::to_string(&ErrorCode::BudgetExceeded).unwrap();
        assert_eq!(json, "\"BUDGET_EXCEEDED\"");
        let ec: ErrorCode = serde_json::from_str(&json).unwrap();
        assert_eq!(ec, ErrorCode::BudgetExceeded);
    }

    #[test]
    fn error_code_retryable() {
        assert!(ErrorCode::InternalError.is_retryable());
        assert!(ErrorCode::Unknown.is_retryable());
        assert!(!ErrorCode::BudgetExceeded.is_retryable());
    }
}
