//! Async HTTP client for the Cycles API.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{Map as JsonMap, Value};

use crate::config::{CyclesClientBuilder, CyclesConfig};
use crate::constants::{
    API_KEY_HEADER, BALANCES_PATH, DECIDE_PATH, EVENTS_PATH, IDEMPOTENCY_KEY_HEADER,
    RESERVATIONS_PATH,
};
use crate::error::Error;
use crate::guard::ReservationGuard;
use crate::journal::{now_ms, CommitJournal, JournalMode, PendingCommitRecord};
use crate::models::enums::Unit;
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

/// Marker prefix the server emits when a reservation targets a scope for which
/// no budget exists at the requested unit. The server indexes budgets by the
/// composite key `(scope, unit)`, so a scope that has an active budget in one
/// unit (e.g. `USD_MICROCENTS`) surfaces as a `NOT_FOUND` when the client
/// reserves in a different unit (e.g. `TOKENS`). The raw 404 message then
/// reads like a plain scope-lookup miss, which is misleading. See issue #8.
const BUDGET_NOT_FOUND_MARKER: &str = "Budget not found for provided scope";

enum ReplayOutcome {
    Terminal,
    Retain(Option<Duration>),
}

pub(crate) fn is_expired_commit(error: &Error) -> bool {
    error.error_code() == Some(ErrorCode::ReservationExpired) || error.status() == Some(410)
}

pub(crate) fn requires_durable_replay(error: &Error) -> bool {
    match error {
        Error::Transport(_) | Error::Deserialization(_) | Error::CommitPending { .. } => true,
        Error::Api { code, .. } => {
            error.is_retryable()
                || error.is_auth_error()
                || code.is_none()
                || *code == Some(ErrorCode::Unknown)
        }
        Error::BudgetExceeded { .. } => error.is_retryable(),
        Error::CommitRecoveryFailed { event_error, .. } => requires_durable_replay(event_error),
        // A validation failure while decoding a successful fallback is
        // ambiguous. Generated request validation happens before journaling,
        // so retaining here cannot make a caller input error retry forever.
        Error::Validation(_) => true,
        Error::Config(_) => false,
    }
}

pub(crate) fn is_settlement_retryable(error: &Error) -> bool {
    error.is_retryable() || matches!(error, Error::Deserialization(_))
}

pub(crate) fn retry_deadline_ms(delay: Duration) -> u64 {
    let bounded = delay.min(crate::retry::RETRY_AFTER_CAP);
    let milliseconds = u64::try_from(bounded.as_millis()).unwrap_or(u64::MAX);
    now_ms().saturating_add(milliseconds)
}
const CREATE_RESPONSE_FIELDS: &[&str] = &[
    "decision",
    "reservation_id",
    "affected_scopes",
    "expires_at_ms",
    "remaining_ttl_ms",
    "scope_path",
    "reserved",
    "caps",
    "reason_code",
    "retry_after_ms",
    "balances",
    "cycles_evidence",
];
const EXTEND_RESPONSE_FIELDS: &[&str] =
    &["status", "expires_at_ms", "remaining_ttl_ms", "balances"];
const COMMIT_RESPONSE_FIELDS: &[&str] = &[
    "status",
    "charged",
    "released",
    "balances",
    "cycles_evidence",
];
const EVENT_RESPONSE_FIELDS: &[&str] = &["status", "event_id", "charged", "balances"];
const AMOUNT_FIELDS: &[&str] = &["unit", "amount"];
const CAPS_FIELDS: &[&str] = &[
    "max_tokens",
    "max_steps_remaining",
    "tool_allowlist",
    "tool_denylist",
    "cooldown_ms",
];
const BALANCE_FIELDS: &[&str] = &[
    "scope",
    "scope_path",
    "remaining",
    "reserved",
    "spent",
    "allocated",
    "debt",
    "overdraft_limit",
    "is_over_limit",
];
const EVIDENCE_FIELDS: &[&str] = &["evidence_id", "cycles_evidence_url"];

fn has_exact_or_optional_fields(object: &JsonMap<String, Value>, allowed: &[&str]) -> bool {
    object.keys().all(|key| allowed.contains(&key.as_str()))
}

fn is_known_unit(value: &Value) -> bool {
    matches!(
        value.as_str(),
        Some("USD_MICROCENTS" | "TOKENS" | "CREDITS" | "RISK_POINTS")
    )
}

fn is_nonnegative_int64(value: &Value) -> bool {
    value
        .as_u64()
        .is_some_and(|number| number <= i64::MAX as u64)
}

fn is_string_array(value: &Value, max_chars: Option<usize>) -> bool {
    value.as_array().is_some_and(|items| {
        items.iter().all(|item| {
            item.as_str()
                .is_some_and(|text| max_chars.is_none_or(|max| text.chars().count() <= max))
        })
    })
}

fn is_amount(value: &Value, signed: bool) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    has_exact_or_optional_fields(object, AMOUNT_FIELDS)
        && object.len() == AMOUNT_FIELDS.len()
        && object.get("unit").is_some_and(is_known_unit)
        && object
            .get("amount")
            .and_then(Value::as_i64)
            .is_some_and(|amount| signed || amount >= 0)
}

fn is_caps(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if !has_exact_or_optional_fields(object, CAPS_FIELDS) {
        return false;
    }
    ["max_tokens", "max_steps_remaining", "cooldown_ms"]
        .into_iter()
        .all(|key| {
            object
                .get(key)
                .is_none_or(|item| item.as_i64().is_some_and(|number| number >= 0))
        })
        && ["tool_allowlist", "tool_denylist"].into_iter().all(|key| {
            object
                .get(key)
                .is_none_or(|item| is_string_array(item, Some(256)))
        })
}

fn is_balance(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if !has_exact_or_optional_fields(object, BALANCE_FIELDS)
        || !object.get("scope").is_some_and(Value::is_string)
        || !object.get("scope_path").is_some_and(Value::is_string)
        || !object
            .get("remaining")
            .is_some_and(|amount| is_amount(amount, true))
    {
        return false;
    }
    ["reserved", "spent", "allocated", "debt", "overdraft_limit"]
        .into_iter()
        .all(|key| {
            object
                .get(key)
                .is_none_or(|amount| is_amount(amount, false))
        })
        && object.get("is_over_limit").is_none_or(Value::is_boolean)
}

fn is_balances(value: &Value) -> bool {
    value
        .as_array()
        .is_some_and(|balances| balances.iter().all(is_balance))
}

fn is_evidence_ref(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if !has_exact_or_optional_fields(object, EVIDENCE_FIELDS)
        || object.len() != EVIDENCE_FIELDS.len()
    {
        return false;
    }
    let valid_id = object
        .get("evidence_id")
        .and_then(Value::as_str)
        .is_some_and(|id| {
            id.len() == 64
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
    let valid_url = object
        .get("cycles_evidence_url")
        .and_then(Value::as_str)
        .is_some_and(|url| reqwest::Url::parse(url).is_ok());
    valid_id && valid_url
}

fn is_schema_valid_create_body(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if !has_exact_or_optional_fields(object, CREATE_RESPONSE_FIELDS)
        || !matches!(
            object.get("decision").and_then(Value::as_str),
            Some("ALLOW" | "ALLOW_WITH_CAPS" | "DENY")
        )
        || !object
            .get("affected_scopes")
            .is_some_and(|value| is_string_array(value, None))
    {
        return false;
    }
    object.get("reservation_id").is_none_or(Value::is_string)
        && object.get("expires_at_ms").is_none_or(is_nonnegative_int64)
        && object
            .get("remaining_ttl_ms")
            .is_none_or(is_nonnegative_int64)
        && object.get("scope_path").is_none_or(Value::is_string)
        && object
            .get("reserved")
            .is_none_or(|value| is_amount(value, false))
        && object.get("caps").is_none_or(is_caps)
        && object.get("reason_code").is_none_or(|value| {
            value
                .as_str()
                .is_some_and(|reason| reason.chars().count() <= 128)
        })
        && object
            .get("retry_after_ms")
            .is_none_or(|value| value.as_u64().is_some())
        && object.get("balances").is_none_or(is_balances)
        && object.get("cycles_evidence").is_none_or(is_evidence_ref)
}

fn is_schema_valid_extend_body(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    has_exact_or_optional_fields(object, EXTEND_RESPONSE_FIELDS)
        && object.get("status").and_then(Value::as_str) == Some("ACTIVE")
        && object
            .get("expires_at_ms")
            .is_some_and(is_nonnegative_int64)
        && object
            .get("remaining_ttl_ms")
            .is_none_or(is_nonnegative_int64)
        && object.get("balances").is_none_or(is_balances)
}

fn is_schema_valid_commit_body(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    has_exact_or_optional_fields(object, COMMIT_RESPONSE_FIELDS)
        && object.get("status").and_then(Value::as_str) == Some("COMMITTED")
        && object
            .get("charged")
            .is_some_and(|amount| is_amount(amount, false))
        && object
            .get("released")
            .is_none_or(|amount| is_amount(amount, false))
        && object.get("balances").is_none_or(is_balances)
        && object.get("cycles_evidence").is_none_or(is_evidence_ref)
}

fn is_schema_valid_event_body(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    has_exact_or_optional_fields(object, EVENT_RESPONSE_FIELDS)
        && object.get("status").and_then(Value::as_str) == Some("APPLIED")
        && object.get("event_id").is_some_and(Value::is_string)
        && object
            .get("charged")
            .is_none_or(|amount| is_amount(amount, false))
        && object.get("balances").is_none_or(is_balances)
}

fn parse_retry_after_delta(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let seconds = value.parse::<u64>().ok()?;
    let milliseconds = seconds.checked_mul(1_000)?;
    if milliseconds > i64::MAX as u64 {
        return None;
    }
    Some(Duration::from_millis(milliseconds))
}

/// If `err` is a 404 `NOT_FOUND` whose message matches the server's
/// "Budget not found for provided scope" pattern, enrich it with the unit that
/// was sent so unit-mismatch cases are self-diagnosing.
fn enrich_budget_not_found(err: Error, unit: Unit) -> Error {
    match err {
        Error::Api {
            status: 404,
            code: Some(ErrorCode::NotFound),
            message,
            request_id,
            retry_after,
            details,
        } if message.starts_with(BUDGET_NOT_FOUND_MARKER) => {
            let unit_wire = serde_json::to_string(&unit)
                .ok()
                .map(|s| s.trim_matches('"').to_string())
                .unwrap_or_else(|| "UNKNOWN".to_string());
            let enriched = format!(
                "{message} (request was sent with unit={unit_wire}; \
                 verify an ACTIVE budget exists at this scope AND unit — \
                 the server indexes budgets by (scope, unit), so a mismatched \
                 unit surfaces as a 404 NOT_FOUND)"
            );
            Error::Api {
                status: 404,
                code: Some(ErrorCode::NotFound),
                message: enriched,
                request_id,
                retry_after,
                details,
            }
        }
        other => other,
    }
}

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
    journal: Option<CommitJournal>,
    journal_replay_started: AtomicBool,
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
    pub fn builder(api_key: impl Into<String>, base_url: impl Into<String>) -> CyclesClientBuilder {
        CyclesClientBuilder::new(api_key, base_url)
    }

    /// Create a client from a pre-built config.
    pub fn new(config: CyclesConfig) -> Self {
        Self::from_builder(config, None)
    }

    /// Internal constructor used by the builder.
    pub(crate) fn from_builder(config: CyclesConfig, http_client: Option<reqwest::Client>) -> Self {
        let http = http_client.unwrap_or_else(|| {
            reqwest::Client::builder()
                .connect_timeout(config.connect_timeout)
                .timeout(config.connect_timeout.saturating_add(config.read_timeout))
                .build()
                .expect("failed to build HTTP client")
        });

        let journal = CommitJournal::for_config(&config);
        if config.journal_enabled && journal.is_none() {
            tracing::warn!(
                "durable commit journal is enabled but no home or explicit journal directory is available"
            );
        }
        let client = Self {
            inner: Arc::new(ClientInner {
                http,
                config,
                journal,
                journal_replay_started: AtomicBool::new(false),
            }),
        };
        client.start_journal_replay();
        client
    }

    /// Access the client configuration.
    pub fn config(&self) -> &CyclesConfig {
        &self.inner.config
    }

    /// Replay unresolved settlements from the durable journal.
    ///
    /// A client created inside a Tokio runtime starts this once
    /// automatically. Applications can await it explicitly during startup or
    /// graceful shutdown. The return value is the number of journal records
    /// removed after a proven terminal outcome.
    pub async fn flush_pending_commits(&self) -> usize {
        self.inner
            .journal_replay_started
            .store(true, Ordering::Release);
        let Some(journal) = self.inner.journal.clone() else {
            return 0;
        };
        let records = journal.load_pending(&self.inner.config.base_url);
        let mut tasks = tokio::task::JoinSet::new();
        for record in records {
            let client = self.clone();
            tasks.spawn(async move { client.replay_pending_record(record).await });
        }
        let mut settled = 0;
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(true) => settled += 1,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "pending-commit replay task did not complete"
                    );
                }
            }
        }
        settled
    }

    /// Replay unresolved settlements, waiting at most `timeout`.
    ///
    /// Cancellation at the timeout is safe: every unresolved record remains
    /// on disk and will be retried on the next flush or process start.
    pub async fn flush_pending_commits_with_timeout(&self, timeout: Duration) -> usize {
        tokio::time::timeout(timeout, self.flush_pending_commits())
            .await
            .unwrap_or(0)
    }

    fn start_journal_replay(&self) {
        if self.inner.journal.is_none()
            || self
                .inner
                .journal_replay_started
                .swap(true, Ordering::AcqRel)
        {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.inner
                .journal_replay_started
                .store(false, Ordering::Release);
            return;
        };
        let client = self.clone();
        handle.spawn(async move {
            let settled = client.flush_pending_commits().await;
            if settled > 0 {
                tracing::info!(settled, "replayed durable pending-commit journal entries");
            }
        });
    }

    pub(crate) fn journal_pending(&self, record: &PendingCommitRecord) -> bool {
        let Some(journal) = &self.inner.journal else {
            return false;
        };
        match journal.record(record) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    reservation_id = %record.reservation_id,
                    error = %error,
                    "failed to journal unresolved commit; continuing without durability"
                );
                false
            }
        }
    }

    pub(crate) fn discard_pending(&self, reservation_id: &str) {
        if let Some(journal) = &self.inner.journal {
            if let Err(error) = journal.discard(reservation_id) {
                tracing::warn!(
                    reservation_id,
                    error = %error,
                    "failed to remove settled pending-commit journal entry"
                );
            }
        }
    }

    async fn replay_pending_record(&self, mut record: PendingCommitRecord) -> bool {
        if let Some(not_before_ms) = record.not_before_ms {
            let delay_ms = not_before_ms.saturating_sub(now_ms());
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }

        let outcome = match record.mode {
            JournalMode::Commit => {
                let Some(commit) = record.commit_body.clone() else {
                    return false;
                };
                let reservation_id = ReservationId::new(record.reservation_id.clone());
                let result = match self.commit_reservation(&reservation_id, &commit).await {
                    Ok(response) => Ok(response),
                    Err(error) if is_settlement_retryable(&error) => {
                        let client = self.clone();
                        crate::retry::CommitRetryEngine::new(self.config())
                            .retry_observed(self, &reservation_id, &commit, error, |failure| {
                                record.not_before_ms = failure.retry_after().map(retry_deadline_ms);
                                client.journal_pending(&record);
                            })
                            .await
                    }
                    Err(error) => Err(error),
                };
                match result {
                    Ok(_) => ReplayOutcome::Terminal,
                    Err(error) if is_expired_commit(&error) => {
                        let Some(event) = record.event_fallback_body.clone() else {
                            return false;
                        };
                        record.mode = JournalMode::Event;
                        record.not_before_ms = None;
                        if !self.journal_pending(&record) {
                            return false;
                        }
                        self.replay_event(&event, &mut record).await
                    }
                    Err(error) if requires_durable_replay(&error) => {
                        ReplayOutcome::Retain(error.retry_after())
                    }
                    Err(_) => ReplayOutcome::Terminal,
                }
            }
            JournalMode::Event => {
                let Some(event) = record.event_fallback_body.as_ref() else {
                    return false;
                };
                let event = event.clone();
                self.replay_event(&event, &mut record).await
            }
        };

        match outcome {
            ReplayOutcome::Terminal => {
                self.discard_pending(&record.reservation_id);
                true
            }
            ReplayOutcome::Retain(retry_after) => {
                record.not_before_ms = retry_after.map(retry_deadline_ms);
                self.journal_pending(&record);
                false
            }
        }
    }

    async fn replay_event(
        &self,
        event: &EventCreateRequest,
        record: &mut PendingCommitRecord,
    ) -> ReplayOutcome {
        use crate::models::EventStatus;

        let result = match self.create_event(event).await {
            Ok(response) => Ok(response),
            Err(error) if is_settlement_retryable(&error) => {
                let client = self.clone();
                crate::retry::CommitRetryEngine::new(self.config())
                    .retry_event_observed(self, event, error, |failure| {
                        record.not_before_ms = failure.retry_after().map(retry_deadline_ms);
                        client.journal_pending(record);
                    })
                    .await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(response) if response.status == EventStatus::Applied => ReplayOutcome::Terminal,
            Ok(_) => ReplayOutcome::Retain(None),
            Err(error) if requires_durable_replay(&error) => {
                ReplayOutcome::Retain(error.retry_after())
            }
            Err(_) => ReplayOutcome::Terminal,
        }
    }

    fn attempt_timeout(&self) -> Duration {
        self.inner
            .config
            .connect_timeout
            .saturating_add(self.inner.config.read_timeout)
    }

    // ─── High-Level API ──────────────────────────────────────────────

    /// Reserve budget and return an RAII guard.
    ///
    /// The guard must be committed or released. If dropped without either,
    /// a best-effort release is attempted.
    ///
    /// Returns `Err(Error::BudgetExceeded)` if the decision is `Deny`.
    #[tracing::instrument(skip(self, req), fields(cycles.reservation_id, cycles.decision))]
    pub async fn reserve(&self, req: ReservationCreateRequest) -> Result<ReservationGuard, Error> {
        validation::validate_subject(&req.subject)?;
        validation::validate_ttl_ms(req.ttl_ms)?;
        validation::validate_grace_period_ms(req.grace_period_ms)?;
        validation::validate_non_negative(req.estimate.amount, "estimate.amount")?;

        // Round-trip time of the reserve call: when the response carries
        // remaining_ttl_ms (spec PR #148), the heartbeat subtracts this from
        // it to get a floor on the lease actually left at receipt. Rounded
        // up — consumed lease must never be under-counted.
        let (response, create_rtt_ms, create_received_at) = self
            .create_reservation_with_metadata_strict(&req)
            .await
            .map_err(|error| enrich_budget_not_found(error, req.estimate.unit))?;
        let resp = response.into_inner();

        if resp.decision.is_denied() {
            return Err(Error::BudgetExceeded {
                message: resp
                    .reason_code
                    .clone()
                    .unwrap_or_else(|| "budget exceeded".to_string()),
                affected_scopes: resp.affected_scopes.clone(),
                retry_after: resp.retry_after_ms.map(Duration::from_millis),
                request_id: None,
                // Derived from a DENY decision on an HTTP 200 response, not
                // from an HTTP error status.
                status: None,
            });
        }

        // Gate on POSITIVE allowance, not merely non-denial: a FUTURE
        // decision value deserializes to `Decision::Unknown` (the
        // `#[serde(other)]` forward-compat arm), and treating it as an
        // allow — even when a reservation_id happens to be present — would
        // silently commit budget under semantics this client does not
        // understand. Unknown decisions are non-allow by definition
        // (`Decision::is_allowed()`).
        if !resp.decision.is_allowed() {
            return Err(Error::Validation(format!(
                "server returned unrecognized decision {:?}; refusing to \
                 construct a reservation guard (unknown/additive decision \
                 values are treated as non-allow)",
                resp.decision
            )));
        }

        // Spec: reservation_id is present when decision is ALLOW /
        // ALLOW_WITH_CAPS and dry_run=false. A missing id on an allowed
        // decision means a non-conformant server — fail with a typed error
        // instead of panicking.
        let reservation_id = match resp.reservation_id.clone() {
            Some(id) => id,
            None => {
                return Err(Error::Validation(format!(
                    "server returned decision {:?} without a reservation_id; \
                     cannot construct a reservation guard",
                    resp.decision
                )));
            }
        };

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
            resp.remaining_ttl_ms,
            create_rtt_ms,
            create_received_at,
            req.subject.clone(),
            req.action.clone(),
        ))
    }

    // ─── Low-Level API ──────────────────────────────────────────────

    /// Create a budget reservation.
    pub async fn create_reservation(
        &self,
        req: &ReservationCreateRequest,
    ) -> Result<ReservationCreateResponse, Error> {
        self.create_reservation_with_metadata_strict(req)
            .await
            .map(|(response, _rtt_ms, _received_at)| response.into_inner())
            .map_err(|e| enrich_budget_not_found(e, req.estimate.unit))
    }

    /// Create a reservation and return the response with metadata.
    pub async fn create_reservation_with_metadata(
        &self,
        req: &ReservationCreateRequest,
    ) -> Result<ApiResponse<ReservationCreateResponse>, Error> {
        self.create_reservation_with_metadata_strict(req)
            .await
            .map(|(response, _rtt_ms, _received_at)| response)
            .map_err(|e| enrich_budget_not_found(e, req.estimate.unit))
    }

    async fn create_reservation_with_metadata_strict(
        &self,
        req: &ReservationCreateRequest,
    ) -> Result<
        (
            ApiResponse<ReservationCreateResponse>,
            u64,
            tokio::time::Instant,
        ),
        Error,
    > {
        self.start_journal_replay();
        let mut final_error = None;
        for attempt in 0..2 {
            let sent_at = tokio::time::Instant::now();
            match self.create_reservation_attempt(req).await {
                Ok(response) => {
                    let received_at = tokio::time::Instant::now();
                    return Ok((
                        response,
                        crate::heartbeat::ceil_ms(received_at.duration_since(sent_at)),
                        received_at,
                    ));
                }
                Err(error) => {
                    let recoverable =
                        matches!(&error, Error::Transport(_) | Error::Deserialization(_))
                            || matches!(&error, Error::Api { status, .. } if *status >= 500);
                    if attempt == 0 && recoverable {
                        final_error = Some(error);
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Err(final_error.unwrap_or_else(|| {
            Error::Validation("reservation create recovery exhausted".to_string())
        }))
    }

    async fn create_reservation_attempt(
        &self,
        req: &ReservationCreateRequest,
    ) -> Result<ApiResponse<ReservationCreateResponse>, Error> {
        let url = format!("{}{RESERVATIONS_PATH}", self.inner.config.base_url);
        let resp = self
            .inner
            .http
            .post(&url)
            .timeout(self.attempt_timeout())
            .header(API_KEY_HEADER, &self.inner.config.api_key)
            .header(IDEMPOTENCY_KEY_HEADER, req.idempotency_key.as_str())
            .json(req)
            .send()
            .await?;
        let response_headers = resp.headers().clone();
        let status = resp.status().as_u16();
        if status == 200 {
            let value: Value = resp.json().await.map_err(|error| {
                Error::Deserialization(serde::de::Error::custom(format!(
                    "ambiguous create response: HTTP 200 body is not valid JSON: {error}"
                )))
            })?;
            if !is_schema_valid_create_body(&value) {
                return Err(Error::Deserialization(serde::de::Error::custom(
                    "ambiguous create response: HTTP 200 body is not a schema-valid ReservationCreateResponse",
                )));
            }
            let data = serde_json::from_value(value).map_err(Error::Deserialization)?;
            Ok(ApiResponse::from_response(data, &response_headers))
        } else if (200..300).contains(&status) {
            Err(Error::Deserialization(serde::de::Error::custom(format!(
                "ambiguous create response: HTTP {status} is a non-200 2xx"
            ))))
        } else {
            Err(self
                .parse_error_response(status, resp, &response_headers)
                .await)
        }
    }

    /// Commit actual spend against a reservation.
    pub async fn commit_reservation(
        &self,
        id: &ReservationId,
        req: &CommitRequest,
    ) -> Result<CommitResponse, Error> {
        let path = format!("{RESERVATIONS_PATH}/{}/commit", id.as_str());
        self.post_json_strict(
            &path,
            req,
            Some(req.idempotency_key.as_str()),
            200,
            "commit",
            is_schema_valid_commit_body,
        )
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

    /// Extend with the spec's strict field-mode success predicate: only a
    /// **schema-valid HTTP 200** `ReservationExtendResponse` counts as an
    /// observed success. A different 2xx status, or a 200 whose body does
    /// not parse against the schema, is *ambiguous* — surfaced as
    /// [`Error::Deserialization`] so the heartbeat's primary-path recovery
    /// treats it as a transient failure and retries with the same
    /// idempotency key (see the HEARTBEAT GUIDANCE in
    /// `cycles-protocol-v0.yaml`). Non-2xx responses parse into the usual
    /// typed errors (including 429 `Retry-After`).
    pub(crate) async fn extend_reservation_strict(
        &self,
        id: &ReservationId,
        req: &ExtendRequest,
    ) -> Result<ExtendResponse, Error> {
        self.start_journal_replay();
        let url = format!(
            "{}{RESERVATIONS_PATH}/{}/extend",
            self.inner.config.base_url,
            id.as_str()
        );
        let resp = self
            .inner
            .http
            .post(&url)
            .timeout(self.attempt_timeout())
            .header(API_KEY_HEADER, &self.inner.config.api_key)
            .header(IDEMPOTENCY_KEY_HEADER, req.idempotency_key.as_str())
            .json(req)
            .send()
            .await?;
        let response_headers = resp.headers().clone();
        let status = resp.status().as_u16();
        if status == 200 {
            let value: Value = resp.json().await.map_err(|error| {
                Error::Deserialization(serde::de::Error::custom(format!(
                    "ambiguous extend response: HTTP 200 body is not valid JSON: {error}"
                )))
            })?;
            if !is_schema_valid_extend_body(&value) {
                return Err(Error::Deserialization(serde::de::Error::custom(
                    "ambiguous extend response: HTTP 200 body is not a schema-valid ReservationExtendResponse",
                )));
            }
            serde_json::from_value(value).map_err(Error::Deserialization)
        } else if (200..300).contains(&status) {
            Err(Error::Deserialization(serde::de::Error::custom(format!(
                "ambiguous extend response: HTTP {status} is a non-200 2xx and cannot be used to schedule from"
            ))))
        } else {
            Err(self
                .parse_error_response(status, resp, &response_headers)
                .await)
        }
    }

    /// Preflight budget decision check (no reservation created).
    pub async fn decide(&self, req: &DecisionRequest) -> Result<DecisionResponse, Error> {
        self.post_json(DECIDE_PATH, req, Some(req.idempotency_key.as_str()))
            .await
            .map_err(|e| enrich_budget_not_found(e, req.estimate.unit))
    }

    /// Create a direct-debit event (no prior reservation).
    pub async fn create_event(
        &self,
        req: &EventCreateRequest,
    ) -> Result<EventCreateResponse, Error> {
        self.post_json_strict(
            EVENTS_PATH,
            req,
            Some(req.idempotency_key.as_str()),
            201,
            "event",
            is_schema_valid_event_body,
        )
        .await
        .map_err(|e| enrich_budget_not_found(e, req.actual.unit))
    }

    /// List reservations with optional filters.
    pub async fn list_reservations(
        &self,
        params: &ListReservationsParams,
    ) -> Result<ReservationListResponse, Error> {
        self.get_json(RESERVATIONS_PATH, Some(params)).await
    }

    /// Get details of a single reservation.
    pub async fn get_reservation(&self, id: &ReservationId) -> Result<ReservationDetail, Error> {
        let path = format!("{RESERVATIONS_PATH}/{}", id.as_str());
        self.get_json::<(), _>(&path, None).await
    }

    /// Query budget balances for scopes.
    pub async fn get_balances(&self, params: &BalanceParams) -> Result<BalanceResponse, Error> {
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
        let resp: ApiResponse<R> = self
            .post_json_with_metadata(path, body, idempotency_key)
            .await?;
        Ok(resp.into_inner())
    }

    async fn post_json_with_metadata<B: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        idempotency_key: Option<&str>,
    ) -> Result<ApiResponse<R>, Error> {
        self.start_journal_replay();
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
            .timeout(self.attempt_timeout())
            .headers(headers)
            .json(body)
            .send()
            .await?;

        let response_headers = resp.headers().clone();
        let status = resp.status().as_u16();

        if (200..300).contains(&status) {
            let data: R = resp
                .json()
                .await
                .map_err(|e| Error::Deserialization(serde::de::Error::custom(e.to_string())))?;
            Ok(ApiResponse::from_response(data, &response_headers))
        } else {
            Err(self
                .parse_error_response(status, resp, &response_headers)
                .await)
        }
    }

    async fn post_json_strict<B: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        idempotency_key: Option<&str>,
        expected_status: u16,
        operation: &str,
        validate: impl FnOnce(&Value) -> bool,
    ) -> Result<R, Error> {
        self.start_journal_replay();
        let url = format!("{}{}", self.inner.config.base_url, path);
        let api_key = HeaderValue::from_str(&self.inner.config.api_key)
            .map_err(|error| Error::Config(format!("invalid API key header value: {error}")))?;
        let mut request = self
            .inner
            .http
            .post(&url)
            .timeout(self.attempt_timeout())
            .header(API_KEY_HEADER, api_key)
            .json(body);
        if let Some(key) = idempotency_key {
            request = request.header(IDEMPOTENCY_KEY_HEADER, key);
        }

        let resp = request.send().await?;
        let response_headers = resp.headers().clone();
        let status = resp.status().as_u16();
        if status == expected_status {
            let value: Value = resp.json().await.map_err(|error| {
                Error::Deserialization(serde::de::Error::custom(format!(
                    "ambiguous {operation} response: HTTP {expected_status} body is not valid JSON: {error}"
                )))
            })?;
            if !validate(&value) {
                return Err(Error::Deserialization(serde::de::Error::custom(format!(
                    "ambiguous {operation} response: HTTP {expected_status} body is not schema-valid"
                ))));
            }
            serde_json::from_value(value).map_err(|error| {
                Error::Deserialization(serde::de::Error::custom(format!(
                    "ambiguous {operation} response: HTTP {expected_status} body could not be decoded: {error}"
                )))
            })
        } else if (200..300).contains(&status) {
            Err(Error::Deserialization(serde::de::Error::custom(format!(
                "ambiguous {operation} response: HTTP {status} is a non-{expected_status} 2xx"
            ))))
        } else {
            Err(self
                .parse_error_response(status, resp, &response_headers)
                .await)
        }
    }

    async fn get_json<Q: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        query: Option<&Q>,
    ) -> Result<R, Error> {
        self.start_journal_replay();
        let url = format!("{}{}", self.inner.config.base_url, path);

        let mut request = self
            .inner
            .http
            .get(&url)
            .timeout(self.attempt_timeout())
            .header(API_KEY_HEADER, &self.inner.config.api_key);

        if let Some(q) = query {
            request = request.query(q);
        }

        let resp = request.send().await?;
        let response_headers = resp.headers().clone();
        let status = resp.status().as_u16();

        if (200..300).contains(&status) {
            resp.json()
                .await
                .map_err(|e| Error::Deserialization(serde::de::Error::custom(e.to_string())))
        } else {
            Err(self
                .parse_error_response(status, resp, &response_headers)
                .await)
        }
    }

    async fn parse_error_response(
        &self,
        status: u16,
        resp: reqwest::Response,
        headers: &HeaderMap,
    ) -> Error {
        let header_request_id = headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        // `Retry-After` in delta-seconds form (runtime spec v0.1.25.12 sends
        // it on HTTP 429 LIMIT_EXCEEDED). The HTTP-date form is not used by
        // Cycles servers and is ignored. Seconds → Duration (ms internally).
        let retry_after = headers
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after_delta);

        let body: Option<ErrorResponse> = resp.json().await.ok();

        let message = body
            .as_ref()
            .map(|b| b.message.clone())
            .unwrap_or_else(|| format!("HTTP {status}"));

        let error_code: Option<ErrorCode> = body
            .as_ref()
            .and_then(|b| serde_json::from_value(serde_json::Value::String(b.error.clone())).ok());

        let details = body.as_ref().and_then(|b| b.details.clone());

        // Prefer request_id from body, fall back to header
        let request_id = body
            .as_ref()
            .and_then(|b| b.request_id.clone())
            .or(header_request_id);

        // Classify budget-related 409 errors
        if status == 409
            && matches!(
                error_code,
                Some(ErrorCode::BudgetExceeded)
                    | Some(ErrorCode::OverdraftLimitExceeded)
                    | Some(ErrorCode::DebtOutstanding)
            )
        {
            return Error::BudgetExceeded {
                message,
                affected_scopes: vec![],
                retry_after,
                request_id,
                status: Some(status),
            };
        }

        Error::Api {
            status,
            code: error_code,
            message,
            request_id,
            retry_after,
            details,
        }
    }
}

// Compile-time assertion: CyclesClient is Send + Sync.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CyclesClient>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::PendingCommitRecord;
    use crate::models::{Action, Amount, IdempotencyKey, Subject};
    use serde_json::json;

    fn test_event() -> EventCreateRequest {
        EventCreateRequest::builder()
            .idempotency_key(IdempotencyKey::new("idem-test"))
            .subject(Subject {
                tenant: Some("acme".to_string()),
                ..Subject::default()
            })
            .action(Action::new("llm.completion", "test"))
            .actual(Amount::tokens(1))
            .build()
    }

    fn test_commit() -> CommitRequest {
        CommitRequest::builder()
            .idempotency_key(IdempotencyKey::new("idem-test"))
            .actual(Amount::tokens(1))
            .build()
    }

    fn pending_record(mode: JournalMode) -> PendingCommitRecord {
        PendingCommitRecord {
            version: 1,
            reservation_id: "rsv_test".to_string(),
            base_url: "http://localhost".to_string(),
            mode,
            commit_body: Some(test_commit()),
            event_fallback_body: Some(test_event()),
            recorded_at_ms: now_ms(),
            not_before_ms: None,
        }
    }

    fn api_error(status: u16, code: Option<ErrorCode>) -> Error {
        Error::Api {
            status,
            code,
            message: "test".to_string(),
            request_id: None,
            retry_after: None,
            details: None,
        }
    }

    #[test]
    fn durable_replay_classification_covers_all_error_families() {
        let serde_error = serde_json::from_str::<String>("not-json").unwrap_err();
        assert!(requires_durable_replay(&Error::Deserialization(
            serde_error
        )));
        assert!(requires_durable_replay(&Error::CommitPending {
            reservation_id: "rsv".to_string(),
            last_error: Box::new(api_error(503, Some(ErrorCode::InternalError))),
        }));
        assert!(requires_durable_replay(&api_error(
            503,
            Some(ErrorCode::InternalError)
        )));
        assert!(requires_durable_replay(&api_error(
            401,
            Some(ErrorCode::Unauthorized)
        )));
        assert!(requires_durable_replay(&api_error(400, None)));
        assert!(requires_durable_replay(&api_error(
            400,
            Some(ErrorCode::Unknown)
        )));
        assert!(!requires_durable_replay(&api_error(
            400,
            Some(ErrorCode::InvalidRequest)
        )));
        assert!(!requires_durable_replay(&Error::BudgetExceeded {
            message: "budget".to_string(),
            affected_scopes: vec![],
            retry_after: None,
            request_id: None,
            status: Some(409),
        }));
        assert!(requires_durable_replay(&Error::CommitRecoveryFailed {
            reservation_id: "rsv".to_string(),
            commit_error: Box::new(api_error(410, Some(ErrorCode::ReservationExpired),)),
            event_error: Box::new(api_error(503, Some(ErrorCode::InternalError))),
        }));
        assert!(requires_durable_replay(&Error::Validation(
            "ambiguous success".to_string()
        )));
        assert!(!requires_durable_replay(&Error::Config(
            "invalid config".to_string()
        )));
    }

    #[test]
    fn retry_deadline_is_bounded_to_the_fleet_cap() {
        let before = now_ms();
        let deadline = retry_deadline_ms(Duration::from_secs(7200));
        assert!(deadline >= before + 3_599_000);
        assert!(deadline <= now_ms() + 3_600_000);
    }

    #[test]
    fn settlement_retry_classification_includes_ambiguous_successes() {
        let serde_error = serde_json::from_str::<String>("not-json").unwrap_err();
        assert!(is_settlement_retryable(&Error::Deserialization(
            serde_error
        )));
        assert!(is_settlement_retryable(&api_error(
            503,
            Some(ErrorCode::InternalError)
        )));
        assert!(!is_settlement_retryable(&api_error(
            400,
            Some(ErrorCode::InvalidRequest)
        )));
    }

    #[tokio::test]
    async fn flush_without_a_journal_is_a_noop() {
        let client = CyclesClient::builder("key", "http://localhost")
            .journal_enabled(false)
            .build();
        assert_eq!(client.flush_pending_commits().await, 0);
    }

    #[tokio::test]
    async fn malformed_in_memory_replay_records_are_retained() {
        let client = CyclesClient::builder("key", "http://localhost")
            .journal_enabled(false)
            .build();
        let mut missing_commit = pending_record(JournalMode::Commit);
        missing_commit.commit_body = None;
        assert!(!client.replay_pending_record(missing_commit).await);

        let mut missing_event = pending_record(JournalMode::Event);
        missing_event.event_fallback_body = None;
        assert!(!client.replay_pending_record(missing_event).await);
    }

    #[test]
    fn journal_io_failures_are_best_effort() {
        let temp = tempfile::tempdir().unwrap();
        let blocked = temp.path().join("blocked");
        std::fs::write(&blocked, "file").unwrap();
        let client = CyclesClient::builder("key", "http://localhost")
            .journal_dir(&blocked)
            .build();
        assert!(!client.journal_pending(&pending_record(JournalMode::Commit)));
    }

    #[test]
    fn discard_io_failures_are_best_effort() {
        let temp = tempfile::tempdir().unwrap();
        let client = CyclesClient::builder("key", "http://localhost")
            .journal_dir(temp.path())
            .build();
        let identity = temp.path().join(crate::journal::auth_fingerprint(
            "http://localhost",
            "key",
            None,
        ));
        std::fs::create_dir_all(identity.join("rsv_test.json")).unwrap();
        client.discard_pending("rsv_test");
    }

    #[test]
    fn strict_lease_response_validators_cover_full_nested_schema() {
        let balance = json!({
            "scope": "tenant:acme",
            "scope_path": "tenant:acme",
            "remaining": {"unit": "TOKENS", "amount": -1},
            "reserved": {"unit": "TOKENS", "amount": 1},
            "is_over_limit": false
        });
        let create = json!({
            "decision": "ALLOW",
            "reservation_id": "rsv_strict",
            "affected_scopes": ["tenant:acme"],
            "remaining_ttl_ms": 0,
            "balances": [balance.clone()],
            "cycles_evidence": {
                "evidence_id": "a".repeat(64),
                "cycles_evidence_url": "https://cycles.example/v1/evidence/id"
            }
        });
        assert!(is_schema_valid_create_body(&create));

        let extend = json!({
            "status": "ACTIVE",
            "expires_at_ms": 1,
            "remaining_ttl_ms": 0,
            "balances": [balance]
        });
        assert!(is_schema_valid_extend_body(&extend));

        let mut extra = extend.clone();
        extra["unexpected"] = json!(true);
        assert!(!is_schema_valid_extend_body(&extra));

        let mut unknown_status = extend.clone();
        unknown_status["status"] = json!("FUTURE");
        assert!(!is_schema_valid_extend_body(&unknown_status));

        let mut negative = extend.clone();
        negative["remaining_ttl_ms"] = json!(-1);
        assert!(!is_schema_valid_extend_body(&negative));

        let mut overflow = extend.clone();
        overflow["remaining_ttl_ms"] = json!(i64::MAX as u64 + 1);
        assert!(!is_schema_valid_extend_body(&overflow));

        let mut malformed_balance = extend;
        malformed_balance["balances"] = json!([{"scope": "missing-required-fields"}]);
        assert!(!is_schema_valid_extend_body(&malformed_balance));

        let commit = json!({
            "status": "COMMITTED",
            "charged": {"unit": "TOKENS", "amount": 1},
            "released": {"unit": "TOKENS", "amount": 0},
            "cycles_evidence": {
                "evidence_id": "a".repeat(64),
                "cycles_evidence_url": "https://cycles.example/v1/evidence/id"
            }
        });
        assert!(is_schema_valid_commit_body(&commit));
        let mut invalid_commit = commit;
        invalid_commit["status"] = json!("FUTURE");
        assert!(!is_schema_valid_commit_body(&invalid_commit));

        let event = json!({
            "status": "APPLIED",
            "event_id": "evt_strict",
            "charged": {"unit": "TOKENS", "amount": 1}
        });
        assert!(is_schema_valid_event_body(&event));
        let mut invalid_event = event;
        invalid_event["extra"] = json!(true);
        assert!(!is_schema_valid_event_body(&invalid_event));
    }

    #[test]
    fn strict_create_validator_rejects_invalid_optional_fields() {
        let valid = json!({
            "decision": "DENY",
            "affected_scopes": [],
            "reason_code": "x".repeat(128)
        });
        assert!(is_schema_valid_create_body(&valid));

        for invalid in [
            json!({"decision": "FUTURE", "affected_scopes": []}),
            json!({"decision": "DENY", "affected_scopes": [], "caps": null}),
            json!({"decision": "DENY", "affected_scopes": [], "expires_at_ms": -1}),
            json!({"decision": "DENY", "affected_scopes": [], "reason_code": "x".repeat(129)}),
            json!({"decision": "DENY", "affected_scopes": [], "reserved": {"unit": "UNKNOWN", "amount": 1}}),
            json!({"decision": "DENY", "affected_scopes": [], "cycles_evidence": {
                "evidence_id": "A".repeat(64),
                "cycles_evidence_url": "relative"
            }}),
        ] {
            assert!(!is_schema_valid_create_body(&invalid), "{invalid}");
        }
    }

    #[test]
    fn retry_after_accepts_only_ascii_delta_seconds() {
        assert_eq!(parse_retry_after_delta(" 3 "), Some(Duration::from_secs(3)));
        let max_seconds = i64::MAX as u64 / 1_000;
        assert_eq!(
            parse_retry_after_delta(&max_seconds.to_string()),
            Some(Duration::from_millis(max_seconds * 1_000))
        );
        assert_eq!(
            parse_retry_after_delta(&(max_seconds + 1).to_string()),
            None
        );
        for value in [
            "",
            "-1",
            "+1",
            "1e2",
            "1.5",
            "Wed, 21 Oct 2026 07:28:00 GMT",
            "9223372036854775808",
        ] {
            assert_eq!(parse_retry_after_delta(value), None, "{value}");
        }
    }

    #[test]
    fn enrich_budget_not_found_adds_unit_hint() {
        let err = Error::Api {
            status: 404,
            code: Some(ErrorCode::NotFound),
            message: "Budget not found for provided scope: tenant:rider".to_string(),
            request_id: Some("req-1".to_string()),
            retry_after: None,
            details: None,
        };
        let enriched = enrich_budget_not_found(err, Unit::Tokens);
        match enriched {
            Error::Api {
                status,
                code,
                message,
                request_id,
                ..
            } => {
                assert_eq!(status, 404);
                assert_eq!(code, Some(ErrorCode::NotFound));
                assert!(message.starts_with("Budget not found for provided scope: tenant:rider"));
                assert!(message.contains("unit=TOKENS"));
                assert!(message.contains("(scope, unit)"));
                assert_eq!(request_id.as_deref(), Some("req-1"));
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn enrich_budget_not_found_uses_wire_format_for_unit() {
        let err = Error::Api {
            status: 404,
            code: Some(ErrorCode::NotFound),
            message: "Budget not found for provided scope: tenant:acme".to_string(),
            request_id: None,
            retry_after: None,
            details: None,
        };
        let enriched = enrich_budget_not_found(err, Unit::UsdMicrocents);
        if let Error::Api { message, .. } = enriched {
            assert!(message.contains("unit=USD_MICROCENTS"));
        } else {
            panic!("expected Api error");
        }
    }

    #[test]
    fn enrich_budget_not_found_ignores_non_matching_messages() {
        let err = Error::Api {
            status: 404,
            code: Some(ErrorCode::NotFound),
            message: "Reservation not found: rsv_xyz".to_string(),
            request_id: None,
            retry_after: None,
            details: None,
        };
        let enriched = enrich_budget_not_found(err, Unit::Tokens);
        if let Error::Api { message, .. } = enriched {
            assert_eq!(message, "Reservation not found: rsv_xyz");
            assert!(!message.contains("unit="));
        } else {
            panic!("expected Api error");
        }
    }

    #[test]
    fn enrich_budget_not_found_ignores_non_404_errors() {
        let err = Error::Api {
            status: 409,
            code: Some(ErrorCode::NotFound),
            message: "Budget not found for provided scope: tenant:rider".to_string(),
            request_id: None,
            retry_after: None,
            details: None,
        };
        let enriched = enrich_budget_not_found(err, Unit::Tokens);
        if let Error::Api { message, .. } = enriched {
            // 409 is not enriched — only 404 NOT_FOUND with the server marker is
            assert_eq!(message, "Budget not found for provided scope: tenant:rider");
        } else {
            panic!("expected Api error");
        }
    }

    #[test]
    fn enrich_budget_not_found_passes_through_other_error_kinds() {
        let err = Error::Validation("bad input".to_string());
        let enriched = enrich_budget_not_found(err, Unit::Tokens);
        assert!(matches!(enriched, Error::Validation(msg) if msg == "bad input"));
    }
}
