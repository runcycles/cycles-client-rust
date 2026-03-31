//! # runcycles
//!
//! Rust client for the [Cycles](https://runcycles.io) budget authority protocol.
//!
//! Cycles provides concurrency-safe spend and action control for autonomous
//! agent runtimes. This crate implements the reserve-execute-commit lifecycle
//! with an idiomatic Rust API built around RAII guards and ownership semantics.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use runcycles::{CyclesClient, Error, models::*};
//!
//! # async fn example() -> Result<(), Error> {
//! // Create a client
//! let client = CyclesClient::builder("my-api-key", "http://localhost:7878")
//!     .tenant("acme")
//!     .build();
//!
//! // Reserve budget — returns an RAII guard
//! let guard = client.reserve(
//!     ReservationCreateRequest::builder()
//!         .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
//!         .action(Action::new("llm.completion", "gpt-4o"))
//!         .estimate(Amount::usd_microcents(5000))
//!         .build()
//! ).await?;
//!
//! // Check caps if decision is AllowWithCaps
//! if let Some(caps) = guard.caps() {
//!     println!("max_tokens: {:?}", caps.max_tokens);
//! }
//!
//! // ... perform the guarded operation ...
//!
//! // Commit actual spend (consumes the guard)
//! guard.commit(
//!     CommitRequest::builder()
//!         .actual(Amount::usd_microcents(3200))
//!         .build()
//! ).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Automatic Lifecycle (like Python's `@cycles` or TypeScript's `withCycles`)
//!
//! ```rust,no_run
//! use runcycles::{CyclesClient, with_cycles, WithCyclesConfig, models::*};
//!
//! # async fn example() -> Result<(), runcycles::Error> {
//! # let client = CyclesClient::builder("key", "http://localhost:7878").tenant("acme").build();
//! let reply = with_cycles(
//!     &client,
//!     WithCyclesConfig::new(Amount::tokens(1000))
//!         .action("llm.completion", "gpt-4o")
//!         .subject(Subject { tenant: Some("acme".into()), ..Default::default() }),
//!     |ctx| async move {
//!         let result = "Hello from LLM".to_string();
//!         Ok((result, Amount::tokens(42)))
//!     },
//! ).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Design Principles
//!
//! - **Ownership safety**: `commit(self)` and `release(self)` consume the guard,
//!   making double-commit a compile error.
//! - **`#[must_use]`**: The compiler warns if a guard is ignored.
//! - **RAII cleanup**: Dropping a guard without commit/release triggers a
//!   best-effort release via `tokio::spawn`.
//! - **Type safety**: Newtype IDs prevent mixing `ReservationId` with
//!   `IdempotencyKey`. `#[non_exhaustive]` enums enable forward compatibility.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod blocking;
pub mod client;
pub mod config;
pub(crate) mod constants;
pub mod error;
pub mod guard;
pub(crate) mod heartbeat;
pub mod lifecycle;
pub mod models;
pub mod response;
pub(crate) mod retry;
pub mod validation;

// Re-export primary types at crate root for ergonomic imports.
pub use client::CyclesClient;
pub use config::CyclesConfig;
pub use error::Error;
pub use guard::ReservationGuard;
pub use lifecycle::{with_cycles, WithCyclesConfig};
pub use response::ApiResponse;
