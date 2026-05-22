//! Request types for the Cycles protocol.

use serde::Serialize;

use super::common::{Action, Amount, CyclesMetrics, Subject};
use super::enums::CommitOveragePolicy;
use super::ids::IdempotencyKey;

/// Request to create a budget reservation.
#[derive(Debug, Clone, Serialize, bon::Builder)]
pub struct ReservationCreateRequest {
    /// Idempotency key for safe retries. Auto-generated if not provided.
    #[builder(default = IdempotencyKey::random())]
    pub idempotency_key: IdempotencyKey,
    /// Who is spending.
    pub subject: Subject,
    /// What is being done.
    pub action: Action,
    /// Estimated cost to reserve.
    pub estimate: Amount,
    /// Time-to-live in milliseconds (default: 60000).
    #[builder(default = 60_000)]
    pub ttl_ms: u64,
    /// Grace period in milliseconds after TTL before expiry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grace_period_ms: Option<u64>,
    /// Policy when actual exceeds estimate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_policy: Option<CommitOveragePolicy>,
    /// If true, evaluate the decision without creating a reservation.
    #[builder(default)]
    #[serde(skip_serializing_if = "is_false")]
    pub dry_run: bool,
    /// Arbitrary metadata to attach.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Request to commit (record actual spend) against a reservation.
#[derive(Debug, Clone, Serialize, bon::Builder)]
pub struct CommitRequest {
    /// Idempotency key for safe retries.
    #[builder(default = IdempotencyKey::random())]
    pub idempotency_key: IdempotencyKey,
    /// The actual amount spent.
    pub actual: Amount,
    /// Optional operation metrics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<CyclesMetrics>,
    /// Arbitrary metadata to attach.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Request to release (cancel) a reservation.
#[derive(Debug, Clone, Serialize)]
pub struct ReleaseRequest {
    /// Idempotency key for safe retries.
    pub idempotency_key: IdempotencyKey,
    /// Optional reason for the release.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ReleaseRequest {
    /// Create a new release request with an auto-generated idempotency key.
    pub fn new(reason: Option<String>) -> Self {
        Self {
            idempotency_key: IdempotencyKey::random(),
            reason,
        }
    }
}

/// Request to extend the TTL of a reservation (heartbeat).
#[derive(Debug, Clone, Serialize)]
pub struct ExtendRequest {
    /// Idempotency key for safe retries.
    pub idempotency_key: IdempotencyKey,
    /// How many milliseconds to extend the TTL by.
    pub extend_by_ms: u64,
    /// Arbitrary metadata to attach.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl ExtendRequest {
    /// Create a new extend request with an auto-generated idempotency key.
    pub fn new(extend_by_ms: u64) -> Self {
        Self {
            idempotency_key: IdempotencyKey::random(),
            extend_by_ms,
            metadata: None,
        }
    }
}

/// Request for a preflight budget decision (no reservation created).
#[derive(Debug, Clone, Serialize, bon::Builder)]
pub struct DecisionRequest {
    /// Idempotency key for safe retries.
    #[builder(default = IdempotencyKey::random())]
    pub idempotency_key: IdempotencyKey,
    /// Who is spending.
    pub subject: Subject,
    /// What is being done.
    pub action: Action,
    /// Estimated cost.
    pub estimate: Amount,
    /// Arbitrary metadata to attach.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Request to create a direct-debit event (no prior reservation).
#[derive(Debug, Clone, Serialize, bon::Builder)]
pub struct EventCreateRequest {
    /// Idempotency key for safe retries.
    #[builder(default = IdempotencyKey::random())]
    pub idempotency_key: IdempotencyKey,
    /// Who is spending.
    pub subject: Subject,
    /// What is being done.
    pub action: Action,
    /// The actual amount to charge.
    pub actual: Amount,
    /// Policy when actual exceeds budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overage_policy: Option<CommitOveragePolicy>,
    /// Optional operation metrics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<CyclesMetrics>,
    /// Client timestamp in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_time_ms: Option<u64>,
    /// Arbitrary metadata to attach.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Query parameters for listing reservations.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ListReservationsParams {
    /// Filter by reservation status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Filter by tenant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Filter by app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Filter by agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Inclusive lower bound on `created_at_ms`. ISO 8601 date-time.
    /// Per cycles-protocol-v0.yaml revision 2026-05-21: the filter always
    /// binds to `created_at_ms` regardless of any sort key; may be supplied
    /// alone (open upper bound) or paired with `to`. Serializes to the
    /// `from` query-string key (the field name already matches the wire
    /// name — no `#[serde(rename)]` needed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Inclusive upper bound on `created_at_ms`. ISO 8601 date-time.
    /// Same binding and open-interval rules as [`from`]. Servers reject
    /// `from > to` with HTTP 400 INVALID_REQUEST. Serializes to the `to`
    /// query-string key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Inclusive lower bound on `expires_at_ms`. ISO 8601 date-time.
    /// Per cycles-protocol-v0.yaml revision 2026-05-22: the filter binds
    /// to `expires_at_ms` independent of [`from`] / [`to`] and of any
    /// `sort_by`. Applies to every row since `expires_at_ms` is required.
    /// May be supplied alone or paired with [`expires_to`]. Servers
    /// reject `expires_from > expires_to` with HTTP 400 INVALID_REQUEST.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_from: Option<String>,
    /// Inclusive upper bound on `expires_at_ms`. ISO 8601 date-time.
    /// Same binding and open-interval rules as [`expires_from`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_to: Option<String>,
    /// Inclusive lower bound on `finalized_at_ms`. ISO 8601 date-time.
    /// Per cycles-protocol-v0.yaml revision 2026-05-22: the filter binds
    /// to `finalized_at_ms`, which is populated only on COMMITTED and
    /// RELEASED rows. ACTIVE and EXPIRED rows are normatively excluded
    /// from results when this is set (their `finalized_at_ms` is absent
    /// so the predicate fails). Callers who want a window over EXPIRED
    /// rows should use [`expires_from`] / [`expires_to`] instead.
    /// Servers reject `finalized_from > finalized_to` with HTTP 400.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finalized_from: Option<String>,
    /// Inclusive upper bound on `finalized_at_ms`. ISO 8601 date-time.
    /// Same binding, open-interval, and ACTIVE/EXPIRED exclusion rules
    /// as [`finalized_from`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finalized_to: Option<String>,
    /// Cursor for pagination.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Maximum results to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Query parameters for retrieving balances.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BalanceParams {
    /// Filter by tenant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Filter by workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Filter by app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Filter by workflow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// Filter by agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Filter by toolset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolset: Option<String>,
}

impl BalanceParams {
    /// Returns `true` if at least one filter field is set.
    pub fn has_filter(&self) -> bool {
        self.tenant.is_some()
            || self.workspace.is_some()
            || self.app.is_some()
            || self.workflow.is_some()
            || self.agent.is_some()
            || self.toolset.is_some()
    }
}

fn is_false(v: &bool) -> bool {
    !v
}
