//! Automatic lifecycle wrapper — the Rust equivalent of Python's `@cycles`
//! decorator and TypeScript's `withCycles` higher-order function.
//!
//! `with_cycles` wraps an async closure with the full reserve → execute →
//! commit/release lifecycle. On success, it commits actual spend; on error,
//! it releases the reservation automatically.
//!
//! # Example
//!
//! ```rust,no_run
//! use runcycles::{CyclesClient, with_cycles, WithCyclesConfig, models::*};
//!
//! # async fn example() -> Result<(), runcycles::Error> {
//! let client = CyclesClient::builder("key", "http://localhost:7878")
//!     .tenant("acme")
//!     .build();
//!
//! // One-liner: reserve → execute → commit (like @cycles in Python)
//! let result = with_cycles(
//!     &client,
//!     WithCyclesConfig::new(Amount::tokens(1000))
//!         .action("llm.completion", "gpt-4o")
//!         .subject(Subject { tenant: Some("acme".into()), ..Default::default() }),
//!     |ctx| async move {
//!         // Access caps if needed
//!         let _caps = ctx.caps;
//!         // Call your LLM
//!         let response = "hello world".to_string();
//!         // Return (result, actual_cost)
//!         Ok((response, Amount::tokens(42)))
//!     },
//! ).await?;
//!
//! println!("LLM said: {}", result);
//! # Ok(())
//! # }
//! ```

use std::future::Future;

use crate::client::CyclesClient;
use crate::error::Error;
use crate::models::common::{Action, Amount, CyclesMetrics, Subject};
use crate::models::enums::CommitOveragePolicy;
use crate::models::request::{CommitRequest, ReservationCreateRequest};

/// Snapshot of guard data passed to the `with_cycles` closure.
///
/// This is a lightweight, owned copy of the reservation state so the
/// closure doesn't need to borrow the guard (avoiding lifetime issues
/// with async closures).
#[derive(Debug, Clone)]
pub struct GuardContext {
    /// The budget decision.
    pub decision: crate::models::Decision,
    /// Soft constraints, if any.
    pub caps: Option<crate::models::Caps>,
    /// The reservation ID.
    pub reservation_id: crate::models::ReservationId,
    /// Scopes affected by this reservation.
    pub affected_scopes: Vec<String>,
}

/// Configuration for [`with_cycles`].
///
/// Builder-style configuration that mirrors the Python `@cycles` decorator
/// parameters and the TypeScript `WithCyclesConfig`.
pub struct WithCyclesConfig {
    estimate: Amount,
    subject: Option<Subject>,
    action_kind: String,
    action_name: String,
    action_tags: Option<Vec<String>>,
    ttl_ms: u64,
    grace_period_ms: Option<u64>,
    overage_policy: Option<CommitOveragePolicy>,
    metrics: Option<CyclesMetrics>,
}

impl WithCyclesConfig {
    /// Create a new config with the estimated cost.
    ///
    /// ```rust
    /// use runcycles::{WithCyclesConfig, models::Amount};
    /// let cfg = WithCyclesConfig::new(Amount::tokens(1000));
    /// ```
    pub fn new(estimate: Amount) -> Self {
        Self {
            estimate,
            subject: None,
            action_kind: "unknown".into(),
            action_name: "unknown".into(),
            action_tags: None,
            ttl_ms: 60_000,
            grace_period_ms: None,
            overage_policy: None,
            metrics: None,
        }
    }

    /// Set the action kind and name (e.g., `"llm.completion"`, `"gpt-4o"`).
    pub fn action(mut self, kind: impl Into<String>, name: impl Into<String>) -> Self {
        self.action_kind = kind.into();
        self.action_name = name.into();
        self
    }

    /// Set the subject (who is spending).
    pub fn subject(mut self, subject: Subject) -> Self {
        self.subject = Some(subject);
        self
    }

    /// Set the TTL in milliseconds (default: 60000).
    pub fn ttl_ms(mut self, ttl_ms: u64) -> Self {
        self.ttl_ms = ttl_ms;
        self
    }

    /// Set the grace period in milliseconds.
    pub fn grace_period_ms(mut self, grace_period_ms: u64) -> Self {
        self.grace_period_ms = Some(grace_period_ms);
        self
    }

    /// Set the overage policy.
    pub fn overage_policy(mut self, policy: CommitOveragePolicy) -> Self {
        self.overage_policy = Some(policy);
        self
    }

    /// Set action tags.
    pub fn action_tags(mut self, tags: Vec<String>) -> Self {
        self.action_tags = Some(tags);
        self
    }

    /// Set metrics to attach to the commit.
    pub fn metrics(mut self, metrics: CyclesMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

/// Execute an async function with automatic Cycles budget enforcement.
///
/// This is the Rust equivalent of Python's `@cycles` decorator and
/// TypeScript's `withCycles` higher-order function.
///
/// The closure receives a reference to the [`ReservationGuard`] (for accessing
/// caps, reservation ID, etc.) and must return `Ok((result, actual_cost))` on
/// success or `Err` on failure.
///
/// - On **success**: commits with the returned `actual_cost`, returns `result`.
/// - On **error**: releases the reservation automatically, propagates the error.
///
/// # Example
///
/// ```rust,no_run
/// use runcycles::{CyclesClient, with_cycles, WithCyclesConfig, models::*};
///
/// # async fn example() -> Result<(), runcycles::Error> {
/// let client = CyclesClient::builder("key", "http://localhost:7878")
///     .tenant("acme")
///     .build();
///
/// let reply = with_cycles(
///     &client,
///     WithCyclesConfig::new(Amount::tokens(1000))
///         .action("llm.completion", "gpt-4o")
///         .subject(Subject { tenant: Some("acme".into()), ..Default::default() }),
///     |ctx| async move {
///         // Check caps
///         if let Some(caps) = &ctx.caps {
///             println!("max_tokens: {:?}", caps.max_tokens);
///         }
///         // Call your LLM here
///         let result = "Hello, world!".to_string();
///         let actual = Amount::tokens(42);
///         Ok((result, actual))
///     },
/// ).await?;
///
/// println!("Got: {reply}");
/// # Ok(())
/// # }
/// ```
pub async fn with_cycles<F, Fut, T>(
    client: &CyclesClient,
    config: WithCyclesConfig,
    f: F,
) -> Result<T, Error>
where
    F: FnOnce(GuardContext) -> Fut,
    Fut: Future<Output = Result<(T, Amount), Box<dyn std::error::Error + Send + Sync>>>,
{
    let subject = config.subject.unwrap_or_default();

    let reserve_req = ReservationCreateRequest {
        idempotency_key: crate::models::IdempotencyKey::random(),
        subject,
        action: Action {
            kind: config.action_kind,
            name: config.action_name,
            tags: config.action_tags,
        },
        estimate: config.estimate,
        ttl_ms: config.ttl_ms,
        grace_period_ms: config.grace_period_ms,
        overage_policy: config.overage_policy,
        dry_run: false,
        metadata: None,
    };

    let guard = client.reserve(reserve_req).await?;

    // Create an owned snapshot so the closure doesn't borrow the guard.
    let ctx = GuardContext {
        decision: guard.decision(),
        caps: guard.caps().cloned(),
        reservation_id: guard.reservation_id().clone(),
        affected_scopes: guard.affected_scopes().to_vec(),
    };

    match f(ctx).await {
        Ok((result, actual)) => {
            let commit_req = CommitRequest {
                idempotency_key: crate::models::IdempotencyKey::random(),
                actual,
                metrics: config.metrics,
                metadata: None,
            };
            guard.commit(commit_req).await?;
            Ok(result)
        }
        Err(e) => {
            let _ = guard.release(format!("guarded_function_failed: {e}")).await;
            Err(Error::Validation(format!("guarded function failed: {e}")))
        }
    }
}
