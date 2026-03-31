//! Protocol enumerations.
//!
//! All enums are `#[non_exhaustive]` for forward compatibility with future
//! protocol versions. Enums that appear in server responses use `#[serde(other)]`
//! on an `Unknown` variant so deserialization never fails on new values.

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
    /// An unrecognized decision value from a newer protocol version.
    #[serde(other)]
    Unknown,
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
    /// An unrecognized unit from a newer protocol version.
    #[serde(other)]
    Unknown,
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
    /// An unrecognized status from a newer protocol version.
    #[serde(other)]
    Unknown,
}

/// Status returned after a successful commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum CommitStatus {
    /// The reservation was committed successfully.
    Committed,
    /// An unrecognized status from a newer protocol version.
    #[serde(other)]
    Unknown,
}

/// Status returned after a successful release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum ReleaseStatus {
    /// The reservation was released successfully.
    Released,
    /// An unrecognized status from a newer protocol version.
    #[serde(other)]
    Unknown,
}

/// Status returned after a successful TTL extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum ExtendStatus {
    /// The reservation is still active with extended TTL.
    Active,
    /// An unrecognized status from a newer protocol version.
    #[serde(other)]
    Unknown,
}

/// Status returned after a successful event (direct debit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum EventStatus {
    /// The event was applied successfully.
    Applied,
    /// An unrecognized status from a newer protocol version.
    #[serde(other)]
    Unknown,
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
    /// An unknown error code from a newer protocol version.
    #[serde(other)]
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
        assert!(!Decision::Unknown.is_allowed());
        assert!(Decision::Deny.is_denied());
        assert!(!Decision::Allow.is_denied());
        assert!(!Decision::Unknown.is_denied());
    }

    #[test]
    fn serde_roundtrip_decision() {
        let json = serde_json::to_string(&Decision::AllowWithCaps).unwrap();
        assert_eq!(json, "\"ALLOW_WITH_CAPS\"");
        let d: Decision = serde_json::from_str(&json).unwrap();
        assert_eq!(d, Decision::AllowWithCaps);
    }

    #[test]
    fn serde_unknown_decision_fallback() {
        let d: Decision = serde_json::from_str("\"ALLOW_WITH_WARNINGS\"").unwrap();
        assert_eq!(d, Decision::Unknown);
    }

    #[test]
    fn serde_roundtrip_unit() {
        let json = serde_json::to_string(&Unit::UsdMicrocents).unwrap();
        assert_eq!(json, "\"USD_MICROCENTS\"");
        let u: Unit = serde_json::from_str(&json).unwrap();
        assert_eq!(u, Unit::UsdMicrocents);
    }

    #[test]
    fn serde_unknown_unit_fallback() {
        let u: Unit = serde_json::from_str("\"ENERGY_JOULES\"").unwrap();
        assert_eq!(u, Unit::Unknown);
    }

    #[test]
    fn serde_roundtrip_all_units() {
        for (variant, expected) in [
            (Unit::UsdMicrocents, "\"USD_MICROCENTS\""),
            (Unit::Tokens, "\"TOKENS\""),
            (Unit::Credits, "\"CREDITS\""),
            (Unit::RiskPoints, "\"RISK_POINTS\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let round: Unit = serde_json::from_str(&json).unwrap();
            assert_eq!(round, variant);
        }
    }

    #[test]
    fn serde_roundtrip_error_code() {
        let json = serde_json::to_string(&ErrorCode::BudgetExceeded).unwrap();
        assert_eq!(json, "\"BUDGET_EXCEEDED\"");
        let ec: ErrorCode = serde_json::from_str(&json).unwrap();
        assert_eq!(ec, ErrorCode::BudgetExceeded);
    }

    #[test]
    fn serde_roundtrip_all_error_codes() {
        let codes = [
            (ErrorCode::InvalidRequest, "\"INVALID_REQUEST\""),
            (ErrorCode::Unauthorized, "\"UNAUTHORIZED\""),
            (ErrorCode::Forbidden, "\"FORBIDDEN\""),
            (ErrorCode::NotFound, "\"NOT_FOUND\""),
            (ErrorCode::BudgetExceeded, "\"BUDGET_EXCEEDED\""),
            (ErrorCode::BudgetFrozen, "\"BUDGET_FROZEN\""),
            (ErrorCode::BudgetClosed, "\"BUDGET_CLOSED\""),
            (ErrorCode::ReservationExpired, "\"RESERVATION_EXPIRED\""),
            (ErrorCode::ReservationFinalized, "\"RESERVATION_FINALIZED\""),
            (ErrorCode::IdempotencyMismatch, "\"IDEMPOTENCY_MISMATCH\""),
            (ErrorCode::UnitMismatch, "\"UNIT_MISMATCH\""),
            (ErrorCode::OverdraftLimitExceeded, "\"OVERDRAFT_LIMIT_EXCEEDED\""),
            (ErrorCode::DebtOutstanding, "\"DEBT_OUTSTANDING\""),
            (ErrorCode::MaxExtensionsExceeded, "\"MAX_EXTENSIONS_EXCEEDED\""),
            (ErrorCode::InternalError, "\"INTERNAL_ERROR\""),
        ];
        for (variant, expected) in codes {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected, "failed for {:?}", variant);
            let round: ErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(round, variant);
        }
    }

    #[test]
    fn serde_unknown_error_code_fallback() {
        let ec: ErrorCode = serde_json::from_str("\"RATE_LIMITED\"").unwrap();
        assert_eq!(ec, ErrorCode::Unknown);
    }

    #[test]
    fn error_code_retryable() {
        assert!(ErrorCode::InternalError.is_retryable());
        assert!(ErrorCode::Unknown.is_retryable());
        assert!(!ErrorCode::BudgetExceeded.is_retryable());
        assert!(!ErrorCode::Forbidden.is_retryable());
        assert!(!ErrorCode::ReservationExpired.is_retryable());
    }

    #[test]
    fn serde_roundtrip_commit_overage_policy() {
        let policies = [
            (CommitOveragePolicy::Reject, "\"REJECT\""),
            (CommitOveragePolicy::AllowIfAvailable, "\"ALLOW_IF_AVAILABLE\""),
            (CommitOveragePolicy::AllowWithOverdraft, "\"ALLOW_WITH_OVERDRAFT\""),
        ];
        for (variant, expected) in policies {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let round: CommitOveragePolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(round, variant);
        }
    }

    #[test]
    fn serde_roundtrip_reservation_status() {
        let statuses = [
            (ReservationStatus::Active, "\"ACTIVE\""),
            (ReservationStatus::Committed, "\"COMMITTED\""),
            (ReservationStatus::Released, "\"RELEASED\""),
            (ReservationStatus::Expired, "\"EXPIRED\""),
        ];
        for (variant, expected) in statuses {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let round: ReservationStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(round, variant);
        }
    }

    #[test]
    fn serde_unknown_reservation_status_fallback() {
        let s: ReservationStatus = serde_json::from_str("\"PENDING\"").unwrap();
        assert_eq!(s, ReservationStatus::Unknown);
    }

    #[test]
    fn serde_roundtrip_single_value_statuses() {
        // CommitStatus
        let json = serde_json::to_string(&CommitStatus::Committed).unwrap();
        assert_eq!(json, "\"COMMITTED\"");
        let cs: CommitStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(cs, CommitStatus::Committed);

        // ReleaseStatus
        let json = serde_json::to_string(&ReleaseStatus::Released).unwrap();
        assert_eq!(json, "\"RELEASED\"");
        let rs: ReleaseStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(rs, ReleaseStatus::Released);

        // ExtendStatus
        let json = serde_json::to_string(&ExtendStatus::Active).unwrap();
        assert_eq!(json, "\"ACTIVE\"");
        let es: ExtendStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(es, ExtendStatus::Active);

        // EventStatus
        let json = serde_json::to_string(&EventStatus::Applied).unwrap();
        assert_eq!(json, "\"APPLIED\"");
        let evs: EventStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(evs, EventStatus::Applied);
    }
}
