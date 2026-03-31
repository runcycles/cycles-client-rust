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
use crate::models::request::{CommitRequest, ReleaseRequest};
use crate::models::response::{CommitResponse, ReleaseResponse};
use crate::models::{Caps, Decision, ReservationId};

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
    finalized: bool,
    cancel: CancellationToken,
    _heartbeat: Option<tokio::task::JoinHandle<()>>,
}

impl ReservationGuard {
    /// Create a new guard (called internally by `CyclesClient::reserve`).
    pub(crate) fn new(
        client: CyclesClient,
        id: ReservationId,
        decision: Decision,
        caps: Option<Caps>,
        expires_at_ms: Option<u64>,
        affected_scopes: Vec<String>,
        ttl_ms: u64,
    ) -> Self {
        let cancel = CancellationToken::new();
        let heartbeat = start_heartbeat(client.clone(), id.clone(), ttl_ms, cancel.clone());

        Self {
            client,
            id,
            decision,
            caps,
            expires_at_ms,
            affected_scopes,
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

    /// Commit actual spend against the reservation.
    ///
    /// Consumes the guard, preventing double-commit at compile time. Stops
    /// the heartbeat and records the actual cost.
    pub async fn commit(mut self, req: CommitRequest) -> Result<CommitResponse, Error> {
        self.finalized = true;
        self.cancel.cancel();
        self.client.commit_reservation(&self.id, &req).await
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
