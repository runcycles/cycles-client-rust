//! Response types from the Cycles protocol.

use serde::Deserialize;

use super::common::{Action, Amount, Balance, Caps, Subject};
use super::enums::{
    CommitStatus, Decision, EventStatus, ExtendStatus, ReleaseStatus, ReservationStatus,
};
use super::ids::{EventId, ReservationId};

/// Reference to a signed CyclesEvidence envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub struct CyclesEvidenceRef {
    /// SHA-256 content identifier of the evidence envelope.
    pub evidence_id: String,
    /// Absolute URL from which the evidence envelope can be fetched.
    pub cycles_evidence_url: String,
}

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
    /// Remaining reservation lifetime in milliseconds at response
    /// evaluation, from the same clock snapshot as `expires_at_ms`
    /// (spec PR #148). Present on successful live-reservation responses;
    /// absent on dry-run/DENY and on older servers. When present, the
    /// heartbeat schedules from it verbatim (normative); when absent, the
    /// grant-ledger heuristic applies.
    #[serde(default)]
    pub remaining_ttl_ms: Option<u64>,
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
    /// Reference to the signed evidence emitted for this reserve operation.
    #[serde(default)]
    pub cycles_evidence: Option<CyclesEvidenceRef>,
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
    /// The direct-debit event that recorded the spend, when the reservation
    /// expired before the commit landed and the client recovered via the
    /// event fallback (`POST /v1/events`).
    ///
    /// **Client-side field** — never populated from a server commit
    /// response. `Some(event_id)` if and only if
    /// [`status`](Self::status) is
    /// [`CommitStatus::RecoveredViaEvent`].
    #[serde(default, skip_deserializing)]
    pub recovered_via_event: Option<EventId>,
}

impl CommitResponse {
    /// Returns `true` if the spend was recorded via the event fallback
    /// rather than a normal reservation commit (see
    /// [`recovered_via_event`](Self::recovered_via_event)).
    pub fn is_recovered_via_event(&self) -> bool {
        self.recovered_via_event.is_some()
    }
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
    /// Remaining reservation lifetime in milliseconds at response
    /// evaluation, from the same clock snapshot as `expires_at_ms`
    /// (spec PR #148). Present on successful responses from servers that
    /// implement it. When present, the heartbeat schedules from it
    /// verbatim (normative); when absent, the grant-ledger heuristic
    /// applies.
    #[serde(default)]
    pub remaining_ttl_ms: Option<u64>,
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
    /// When the reservation reached a terminal state (Unix ms).
    /// Per cycles-protocol-v0.yaml revision 2026-05-22: populated on
    /// COMMITTED and RELEASED rows only; absent on ACTIVE and EXPIRED.
    /// Added to ReservationSummary in revision 2026-05-22 to support
    /// the `finalized_from` / `finalized_to` window filter — callers
    /// filtering on finalization time can see the timestamp directly
    /// in list results without a follow-up `get_reservation` call.
    /// Servers older than v0.1.25.21 do not emit this field; the
    /// `Option` + `#[serde(default)]` make the deserialization
    /// back-compatible.
    #[serde(default)]
    pub finalized_at_ms: Option<u64>,
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
