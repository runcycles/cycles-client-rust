//! Response types from the Cycles protocol.

use serde::Deserialize;

use super::common::{Action, Amount, Balance, Caps, Subject};
use super::enums::{
    CommitStatus, Decision, EventStatus, ExtendStatus, ReleaseStatus, ReservationStatus,
};
use super::ids::{EventId, ReservationId};

/// Response from creating a reservation.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct ReservationCreateResponse {
    /// The budget decision.
    pub decision: Decision,
    /// The reservation ID (present when decision is ALLOW or ALLOW_WITH_CAPS).
    #[serde(default)]
    pub reservation_id: Option<ReservationId>,
    /// Scopes affected by this reservation.
    #[serde(default)]
    pub affected_scopes: Vec<String>,
    /// When the reservation expires (Unix ms).
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
    /// The fully qualified scope path.
    #[serde(default)]
    pub scope_path: Option<String>,
    /// The amount that was reserved.
    #[serde(default)]
    pub reserved: Option<Amount>,
    /// Soft constraints (when decision is ALLOW_WITH_CAPS).
    #[serde(default)]
    pub caps: Option<Caps>,
    /// Reason code for denial.
    #[serde(default)]
    pub reason_code: Option<String>,
    /// Suggested retry delay in milliseconds.
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
    /// Current balances after the reservation.
    #[serde(default)]
    pub balances: Option<Vec<Balance>>,
}

/// Response from committing a reservation.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct CommitResponse {
    /// The commit status.
    pub status: CommitStatus,
    /// The amount charged.
    pub charged: Amount,
    /// The amount released (delta between reserved and actual).
    #[serde(default)]
    pub released: Option<Amount>,
    /// Current balances after the commit.
    #[serde(default)]
    pub balances: Option<Vec<Balance>>,
}

/// Response from releasing a reservation.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct ReleaseResponse {
    /// The release status.
    pub status: ReleaseStatus,
    /// The amount released.
    pub released: Amount,
    /// Current balances after the release.
    #[serde(default)]
    pub balances: Option<Vec<Balance>>,
}

/// Response from extending a reservation's TTL.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct ExtendResponse {
    /// The extend status.
    pub status: ExtendStatus,
    /// The new expiry time (Unix ms).
    pub expires_at_ms: u64,
    /// Current balances.
    #[serde(default)]
    pub balances: Option<Vec<Balance>>,
}

/// Response from a preflight decision check.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct DecisionResponse {
    /// The budget decision.
    pub decision: Decision,
    /// Soft constraints (when decision is ALLOW_WITH_CAPS).
    #[serde(default)]
    pub caps: Option<Caps>,
    /// Reason code for denial.
    #[serde(default)]
    pub reason_code: Option<String>,
    /// Suggested retry delay in milliseconds.
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
    /// Scopes that would be affected.
    #[serde(default)]
    pub affected_scopes: Option<Vec<String>>,
}

/// Response from creating a direct-debit event.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct EventCreateResponse {
    /// The event status.
    pub status: EventStatus,
    /// The assigned event ID.
    pub event_id: EventId,
    /// The amount charged.
    #[serde(default)]
    pub charged: Option<Amount>,
    /// Current balances after the event.
    #[serde(default)]
    pub balances: Option<Vec<Balance>>,
}

/// Detailed information about a single reservation.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct ReservationDetail {
    /// The reservation ID.
    pub reservation_id: ReservationId,
    /// Current status.
    pub status: ReservationStatus,
    /// Who is spending.
    pub subject: Subject,
    /// What is being done.
    pub action: Action,
    /// The reserved amount.
    pub reserved: Amount,
    /// When the reservation was created (Unix ms).
    pub created_at_ms: u64,
    /// When the reservation expires (Unix ms).
    pub expires_at_ms: u64,
    /// The fully qualified scope path.
    pub scope_path: String,
    /// Scopes affected by this reservation.
    pub affected_scopes: Vec<String>,
    /// The idempotency key used.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Amount committed (if committed).
    #[serde(default)]
    pub committed: Option<Amount>,
    /// When the reservation was finalized (Unix ms).
    #[serde(default)]
    pub finalized_at_ms: Option<u64>,
    /// Arbitrary metadata.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

/// Summary information about a reservation (used in list responses).
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct ReservationSummary {
    /// The reservation ID.
    pub reservation_id: ReservationId,
    /// Current status.
    pub status: ReservationStatus,
    /// Who is spending.
    pub subject: Subject,
    /// What is being done.
    pub action: Action,
    /// The reserved amount.
    pub reserved: Amount,
    /// When the reservation was created (Unix ms).
    pub created_at_ms: u64,
    /// When the reservation expires (Unix ms).
    pub expires_at_ms: u64,
    /// The fully qualified scope path.
    pub scope_path: String,
    /// Scopes affected.
    pub affected_scopes: Vec<String>,
    /// The idempotency key used.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// Paginated list of reservations.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct ReservationListResponse {
    /// The reservation summaries.
    pub reservations: Vec<ReservationSummary>,
    /// Whether more results are available.
    #[serde(default)]
    pub has_more: Option<bool>,
    /// Cursor for the next page.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// Paginated list of balances.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct BalanceResponse {
    /// The balance entries.
    pub balances: Vec<Balance>,
    /// Whether more results are available.
    #[serde(default)]
    pub has_more: Option<bool>,
    /// Cursor for the next page.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// Error response from the Cycles server.
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorResponse {
    /// The error code string.
    pub error: String,
    /// Human-readable error message.
    pub message: String,
    /// Request ID for correlation.
    #[serde(default)]
    pub request_id: Option<String>,
    /// Additional error details.
    #[serde(default)]
    pub details: Option<serde_json::Value>,
}

/// Result of a dry-run reservation (decision without creating a reservation).
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct DryRunResult {
    /// The budget decision.
    pub decision: Decision,
    /// Soft constraints.
    #[serde(default)]
    pub caps: Option<Caps>,
    /// Scopes that would be affected.
    #[serde(default)]
    pub affected_scopes: Option<Vec<String>>,
    /// The scope path.
    #[serde(default)]
    pub scope_path: Option<String>,
    /// The amount that would be reserved.
    #[serde(default)]
    pub reserved: Option<Amount>,
    /// Current balances.
    #[serde(default)]
    pub balances: Option<Vec<Balance>>,
    /// Reason code for denial.
    #[serde(default)]
    pub reason_code: Option<String>,
    /// Suggested retry delay.
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
}
