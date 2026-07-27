//! RAII reservation guard for the Cycles protocol lifecycle.
//!
//! The guard is the primary high-level API. It holds a live reservation and
//! provides access to the decision, caps, and affected scopes. The reservation
//! must be committed or released; if the guard is dropped without either, a
//! best-effort release is attempted.
//!
//! # Example
//!
//! ```rust,no_run
//! # use runcycles::{CyclesClient, Error, models::*};
//! # async fn example(client: CyclesClient) -> Result<(), Error> {
//! let guard = client.reserve(
//!     ReservationCreateRequest::builder()
//!         .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
//!         .action(Action::new("llm.completion", "gpt-4o"))
//!         .estimate(Amount::usd_microcents(5000))
//!         .build()
//! ).await?;
//!
//! // Access caps for soft constraints
//! if let Some(caps) = guard.caps() {
//!     println!("max_tokens: {:?}", caps.max_tokens);
//! }
//!
//! // ... perform the guarded operation ...
//!
//! // Commit actual spend (consumes the guard — cannot double-commit)
//! guard.commit(
//!     CommitRequest::builder()
//!         .actual(Amount::usd_microcents(3200))
//!         .build()
//! ).await?;
//! # Ok(())
//! # }
//! ```

use tokio_util::sync::CancellationToken;

use crate::client::CyclesClient;
use crate::error::Error;
use crate::heartbeat::start_heartbeat;
use crate::models::common::{Action, Subject};
use crate::models::enums::{CommitStatus, ErrorCode, EventStatus};
use crate::models::request::{CommitRequest, EventCreateRequest, ReleaseRequest};
use crate::models::response::{CommitResponse, ReleaseResponse};
use crate::models::{Caps, Decision, ReservationId};
use crate::retry::CommitRetryEngine;

/// RAII guard for a live budget reservation.
///
/// Created by [`CyclesClient::reserve`]. The guard provides access to the
/// reservation's decision and caps, and must be finalized by calling either
/// [`commit`](Self::commit) or [`release`](Self::release).
///
/// Both `commit` and `release` take `self` by value, preventing double-use at
/// compile time. If the guard is dropped without being finalized, a best-effort
/// release is attempted via `tokio::spawn`.
#[must_use = "reservation must be committed or released; will auto-release on drop"]
pub struct ReservationGuard {
    // Note: Debug is implemented manually below because JoinHandle doesn't impl Debug nicely.
    client: CyclesClient,
    id: ReservationId,
    decision: Decision,
    caps: Option<Caps>,
    expires_at_ms: Option<u64>,
    affected_scopes: Vec<String>,
    subject: Subject,
    action: Action,
    finalized: bool,
    cancel: CancellationToken,
    _heartbeat: Option<tokio::task::JoinHandle<()>>,
}

impl ReservationGuard {
    /// Create a new guard (called internally by `CyclesClient::reserve`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        client: CyclesClient,
        id: ReservationId,
        decision: Decision,
        caps: Option<Caps>,
        expires_at_ms: Option<u64>,
        affected_scopes: Vec<String>,
        requested_ttl_ms: u64,
        date_ttl_hint_ms: Option<u64>,
        subject: Subject,
        action: Action,
    ) -> Self {
        let cancel = CancellationToken::new();
        // The reserve response's expires_at_ms (server frame) is the base of
        // the heartbeat's grant ledger; the Date-derived TTL estimate is a
        // first-beat cadence hint only (the Date header is not the same
        // clock as expires_at_ms); see src/heartbeat.rs module docs.
        let heartbeat = start_heartbeat(
            client.clone(),
            id.clone(),
            requested_ttl_ms,
            expires_at_ms,
            date_ttl_hint_ms,
            cancel.clone(),
        );

        Self {
            client,
            id,
            decision,
            caps,
            expires_at_ms,
            affected_scopes,
            subject,
            action,
            finalized: false,
            cancel,
            _heartbeat: Some(heartbeat),
        }
    }

    /// The reservation ID.
    pub fn reservation_id(&self) -> &ReservationId {
        &self.id
    }

    /// The budget decision (`Allow` or `AllowWithCaps`).
    pub fn decision(&self) -> Decision {
        self.decision
    }

    /// Soft constraints, if the decision was `AllowWithCaps`.
    pub fn caps(&self) -> Option<&Caps> {
        self.caps.as_ref()
    }

    /// Returns `true` if the decision includes caps.
    pub fn is_capped(&self) -> bool {
        self.decision == Decision::AllowWithCaps
    }

    /// When the reservation expires (Unix milliseconds).
    pub fn expires_at_ms(&self) -> Option<u64> {
        self.expires_at_ms
    }

    /// Scopes affected by this reservation.
    pub fn affected_scopes(&self) -> &[String] {
        &self.affected_scopes
    }

    /// The subject (who is spending) the reservation was created with.
    pub fn subject(&self) -> &Subject {
        &self.subject
    }

    /// The action (what is being done) the reservation was created with.
    pub fn action(&self) -> &Action {
        &self.action
    }

    /// Commit actual spend against the reservation.
    ///
    /// Consumes the guard, preventing double-commit at compile time, and
    /// records the actual cost.
    ///
    /// If the attempt fails with a retryable error — a transport failure, a
    /// 5xx server error, or an error code the protocol classifies as
    /// transient (see [`Error::is_retryable`]) — and
    /// [`retry_enabled`](crate::config::CyclesConfig::retry_enabled) is set,
    /// the commit is retried inline with exponential backoff, reusing the
    /// same request — and therefore the same idempotency key, so a commit
    /// that already landed server-side cannot double-charge. The heartbeat
    /// keeps extending the reservation's TTL until the outcome is final, so
    /// the reservation cannot expire mid-retry.
    ///
    /// The returned result is final: `Ok` means the spend was recorded;
    /// `Err` means no further commit attempt will be made (the retry stopped
    /// on a non-retryable error or exhausted
    /// [`retry_max_attempts`](crate::config::CyclesConfig::retry_max_attempts)),
    /// so the caller may safely compensate. Under a persistent outage this
    /// call blocks for the full backoff schedule; tune the `retry_*` config
    /// knobs or disable retry for fail-fast single-attempt commits.
    ///
    /// # Expired-reservation event fallback
    ///
    /// A commit records spend that **already happened**, so a
    /// `RESERVATION_EXPIRED` rejection (the commit landed after the
    /// reservation's grace period; the server already returned the reserved
    /// budget to the pool) must not silently drop the spend. When the commit
    /// path receives `RESERVATION_EXPIRED` — or any HTTP 410 response, even
    /// one whose body could not be parsed into a typed error code — on the
    /// first attempt or during retry, the guard records the spend as a
    /// post-hoc direct-debit event (`POST /v1/events`, no reservation
    /// needed) instead:
    ///
    /// - the event reuses the commit's **idempotency key**, so the recovery
    ///   is exactly-once even if it races another client;
    /// - subject and action are taken from the reservation the guard holds;
    /// - the commit's metadata is carried over, extended with
    ///   `recovered_reservation_id` (this reservation's ID) and
    ///   `recovery_reason = "commit_after_reservation_expired"` so the ledger
    ///   shows exactly why the spend arrived as an event;
    /// - transient event failures (transport, 5xx) are retried with the same
    ///   bounded backoff policy as commits.
    ///
    /// On fallback success, `commit()` returns `Ok` with
    /// [`CommitStatus::RecoveredViaEvent`] and
    /// [`CommitResponse::recovered_via_event`] set to the recorded event's
    /// ID. On fallback failure, it returns
    /// [`Error::CommitRecoveryFailed`] carrying **both** the original
    /// `RESERVATION_EXPIRED` error and the event error — the spend is not
    /// recorded and the caller must compensate.
    ///
    /// # Cancellation
    ///
    /// Recovery is **not** cancellation-transparent: if the caller times out
    /// or drops this future mid-recovery, the outcome is unknown — the
    /// fallback event may or may not have landed server-side. It is safe to
    /// re-`POST /v1/events` with the commit's idempotency key to settle the
    /// question: the server records the event exactly once per key, so a
    /// re-post either lands the spend or dedupes against the one that
    /// already did.
    pub async fn commit(mut self, req: CommitRequest) -> Result<CommitResponse, Error> {
        self.finalized = true;
        // The heartbeat is deliberately NOT cancelled yet: it must keep the
        // reservation alive while retries run. Cancelled below once the
        // outcome is final (Drop also cancels, as a backstop).
        let result = match self.client.commit_reservation(&self.id, &req).await {
            Ok(resp) => Ok(resp),
            Err(e) if e.is_retryable() => {
                CommitRetryEngine::new(self.client.config())
                    .retry(&self.client, &self.id, &req, e)
                    .await
            }
            Err(e) => Err(e),
        };
        // RESERVATION_EXPIRED is non-retryable (the reservation is gone for
        // good), so it falls through the retry engine unchanged whether it
        // occurred on the first attempt or mid-retry. Recover it here. An
        // HTTP 410 (Gone) triggers recovery even when the body could not be
        // parsed into a typed error code: the status alone means the
        // reservation is settled/expired.
        let result = match result {
            Err(e)
                if e.error_code() == Some(ErrorCode::ReservationExpired)
                    || e.status() == Some(410) =>
            {
                // The reservation is expired for good — stop the heartbeat
                // BEFORE recovery so it doesn't keep sending doomed extend
                // requests (traffic and log noise) while the fallback runs.
                self.cancel.cancel();
                self.recover_via_event(&req, e).await
            }
            other => other,
        };
        self.cancel.cancel();
        result
    }

    /// Record the spend of an expired-reservation commit as a post-hoc
    /// direct-debit event. See [`commit`](Self::commit) for the contract.
    async fn recover_via_event(
        &self,
        req: &CommitRequest,
        commit_error: Error,
    ) -> Result<CommitResponse, Error> {
        tracing::warn!(
            reservation_id = %self.id,
            "commit rejected with RESERVATION_EXPIRED; recovering spend via direct-debit event"
        );

        // Carry the commit metadata over, marked so the ledger shows why the
        // spend arrived as an event. Non-object metadata (legal JSON, if
        // unusual) is preserved under "original_metadata".
        let mut metadata = match req.metadata.clone() {
            Some(serde_json::Value::Object(map)) => map,
            Some(other) => {
                let mut map = serde_json::Map::new();
                map.insert("original_metadata".to_string(), other);
                map
            }
            None => serde_json::Map::new(),
        };
        metadata.insert(
            "recovered_reservation_id".to_string(),
            serde_json::Value::String(self.id.as_str().to_string()),
        );
        metadata.insert(
            "recovery_reason".to_string(),
            serde_json::Value::String("commit_after_reservation_expired".to_string()),
        );

        let event_req = EventCreateRequest {
            // Same key as the commit: the event namespace is separate from
            // the commit namespace server-side, and reusing the key makes
            // the recovery itself exactly-once across retries and racing
            // clients.
            idempotency_key: req.idempotency_key.clone(),
            subject: self.subject.clone(),
            action: self.action.clone(),
            actual: req.actual.clone(),
            // No overage policy: the server default ALLOW_IF_AVAILABLE never
            // rejects, which is what we want — the spend already happened.
            overage_policy: None,
            metrics: req.metrics.clone(),
            client_time_ms: None,
            metadata: Some(serde_json::Value::Object(metadata)),
        };

        let event_result = match self.client.create_event(&event_req).await {
            Ok(resp) => Ok(resp),
            Err(e) if e.is_retryable() => {
                CommitRetryEngine::new(self.client.config())
                    .retry_event(&self.client, &event_req, e)
                    .await
            }
            Err(e) => Err(e),
        };

        // An HTTP success whose status is not APPLIED (including a future
        // status deserialized as `Unknown`) is NOT proof the spend was
        // recorded — refuse to report recovery on semantics this client
        // does not understand.
        let event_result = event_result.and_then(|event| {
            if event.status == EventStatus::Applied {
                Ok(event)
            } else {
                Err(Error::Validation(format!(
                    "event fallback returned HTTP success with unexpected status {:?} \
                     (expected APPLIED); refusing to treat it as recovery",
                    event.status
                )))
            }
        });

        match event_result {
            Ok(event) => {
                tracing::info!(
                    reservation_id = %self.id,
                    event_id = %event.event_id,
                    "expired commit recovered via direct-debit event"
                );
                Ok(CommitResponse {
                    status: CommitStatus::RecoveredViaEvent,
                    charged: event.charged.unwrap_or_else(|| req.actual.clone()),
                    released: None,
                    balances: event.balances,
                    recovered_via_event: Some(event.event_id),
                })
            }
            Err(event_error) => {
                tracing::error!(
                    reservation_id = %self.id,
                    commit_error = %commit_error,
                    event_error = %event_error,
                    "event fallback failed after RESERVATION_EXPIRED; spend is NOT recorded"
                );
                Err(Error::CommitRecoveryFailed {
                    reservation_id: self.id.as_str().to_string(),
                    commit_error: Box::new(commit_error),
                    event_error: Box::new(event_error),
                })
            }
        }
    }

    /// Release the reservation, returning reserved budget.
    ///
    /// Consumes the guard, preventing double-release at compile time.
    pub async fn release(mut self, reason: impl Into<String>) -> Result<ReleaseResponse, Error> {
        self.finalized = true;
        self.cancel.cancel();
        let req = ReleaseRequest::new(Some(reason.into()));
        self.client.release_reservation(&self.id, &req).await
    }

    /// Manually extend the reservation's TTL.
    ///
    /// This is normally handled automatically by the heartbeat. Use this only
    /// if you disabled the heartbeat or need explicit control.
    pub async fn extend(&self, extend_by_ms: u64) -> Result<(), Error> {
        let req = crate::models::ExtendRequest::new(extend_by_ms);
        self.client.extend_reservation(&self.id, &req).await?;
        Ok(())
    }
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        self.cancel.cancel(); // Always stop heartbeat.

        if !self.finalized {
            tracing::warn!(
                reservation_id = %self.id,
                "ReservationGuard dropped without commit/release; attempting best-effort release"
            );

            // Best-effort release. If no tokio runtime is active (e.g., during
            // shutdown), this silently does nothing — the server will expire
            // the reservation via TTL.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let client = self.client.clone();
                let id = self.id.clone();
                handle.spawn(async move {
                    let req = ReleaseRequest::new(Some("guard_dropped".to_string()));
                    let _ = client.release_reservation(&id, &req).await;
                });
            }
        }
    }
}

impl std::fmt::Debug for ReservationGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReservationGuard")
            .field("id", &self.id)
            .field("decision", &self.decision)
            .field("finalized", &self.finalized)
            .finish()
    }
}

// Compile-time assertion: ReservationGuard is Send (can move across .await points).
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<ReservationGuard>();
};
