[![Crates.io](https://img.shields.io/crates/v/runcycles)](https://crates.io/crates/runcycles)
[![docs.rs](https://img.shields.io/docsrs/runcycles)](https://docs.rs/runcycles)
[![CI](https://github.com/runcycles/cycles-client-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/runcycles/cycles-client-rust/actions)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue)](LICENSE)
[![Coverage](https://img.shields.io/badge/coverage-95%25-brightgreen)](https://github.com/runcycles/cycles-client-rust/actions)

# Cycles Rust Client

Rust client for the [Cycles](https://runcycles.io) budget-management protocol — deterministic spend control for AI agents and LLM workflows.

Cycles provides concurrency-safe spend and action control for autonomous agent
runtimes. This crate implements the reserve-execute-commit lifecycle with an
idiomatic Rust API built around RAII guards and ownership semantics.

## Installation

```toml
[dependencies]
runcycles = "0.2"
```

> **Unit must match the budget.** The `Amount` you pass to `reserve`,
> `with_cycles`, `decide`, or `create_event` must be in the same unit as the
> active budget at the target scope. The server indexes budgets by
> `(scope, unit)`, so reserving `Amount::tokens(…)` against a
> `USD_MICROCENTS` budget returns a 404 *"Budget not found for provided
> scope"* even though the scope exists. The client enriches such 404s with
> the unit that was sent to make the mismatch obvious.

## Quick Start — Automatic Lifecycle (`with_cycles`)

Like Python's `@cycles` decorator or TypeScript's `withCycles`. Reserve, execute,
and commit/release are handled automatically:

```rust,no_run
use runcycles::{CyclesClient, with_cycles, WithCyclesConfig, models::*};

#[tokio::main]
async fn main() -> Result<(), runcycles::Error> {
    let client = CyclesClient::builder("my-api-key", "http://localhost:7878")
        .tenant("acme")
        .build();

    let reply = with_cycles(
        &client,
        WithCyclesConfig::new(Amount::tokens(1000))
            .action("llm.completion", "gpt-4o")
            .subject(Subject { tenant: Some("acme".into()), ..Default::default() }),
        |ctx| async move {
            // ctx.caps, ctx.decision, ctx.reservation_id available
            let result = call_llm("Hello").await;
            Ok((result, Amount::tokens(42)))   // (return_value, actual_cost)
        },
    ).await?;
    // On success → auto-commits. On error → auto-releases.

    println!("LLM said: {reply}");
    Ok(())
}
# async fn call_llm(_: &str) -> String { "hi".into() }
```

## Manual Control — RAII Guard

For streaming, multi-step workflows, or when you need full control:

```rust,no_run
use runcycles::{CyclesClient, Error, models::*};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let client = CyclesClient::builder("my-api-key", "http://localhost:7878")
        .tenant("acme")
        .build();

    // Reserve budget — returns an RAII guard
    let guard = client.reserve(
        ReservationCreateRequest::builder()
            .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
            .action(Action::new("llm.completion", "gpt-4o"))
            .estimate(Amount::usd_microcents(5000))
            .build()
    ).await?;

    // Check caps if decision is AllowWithCaps
    if let Some(caps) = guard.caps() {
        println!("max_tokens: {:?}", caps.max_tokens);
    }

    // ... perform the guarded operation ...

    // Commit actual spend (consumes the guard — cannot double-commit)
    guard.commit(
        CommitRequest::builder()
            .actual(Amount::usd_microcents(3200))
            .build()
    ).await?;

    Ok(())
}
```

## Design

The Rust client is not a port — it is designed from the ground up around Rust's
type system and ownership model:

| Feature | How |
|---------|-----|
| **No double-commit** | `commit(self)` consumes the guard — compile error to reuse |
| **No forgotten reservations** | `#[must_use]` warns if guard is ignored |
| **Auto-cleanup** | `Drop` does best-effort release via `tokio::spawn` |
| **Type-safe IDs** | `ReservationId`, `IdempotencyKey` newtypes prevent mixups |
| **Forward-compatible** | `#[non_exhaustive]` enums for protocol evolution |
| **Zero mapper code** | `serde` with `rename_all` handles wire format natively |

### RAII Guard

The `ReservationGuard` gives manual control over the lifecycle. It holds a live
reservation and auto-extends TTL via a background heartbeat. The guard IS the
context — no thread-locals or task-locals needed.

```rust,no_run
# use runcycles::{CyclesClient, Error, models::*};
# async fn example(client: CyclesClient) -> Result<(), Error> {
let guard = client.reserve(/* ... */
# ReservationCreateRequest::builder()
#     .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
#     .action(Action::new("llm.completion", "gpt-4o"))
#     .estimate(Amount::usd_microcents(5000))
#     .build()
).await?;

// The guard provides all context
guard.reservation_id();  // &ReservationId
guard.decision();        // Decision::Allow or AllowWithCaps
guard.caps();            // Option<&Caps>
guard.is_capped();       // bool
guard.affected_scopes(); // &[String]

// Commit or release (both consume `self`)
guard.commit(CommitRequest::builder().actual(Amount::usd_microcents(3200)).build()).await?;
// guard.commit(...) here would be a COMPILE ERROR
# Ok(())
# }
```

### Low-Level Client

For full control, use the client methods directly:

```rust,no_run
# use runcycles::{CyclesClient, models::*};
# async fn example(client: CyclesClient) -> Result<(), runcycles::Error> {
let resp = client.create_reservation(&ReservationCreateRequest::builder()
    .subject(Subject { tenant: Some("acme".into()), ..Default::default() })
    .action(Action::new("llm.completion", "gpt-4o"))
    .estimate(Amount::usd_microcents(5000))
    .build()
).await?;

if resp.decision.is_allowed() {
    let id = resp.reservation_id.unwrap();
    // ... do work ...
    client.commit_reservation(&id, &CommitRequest::builder()
        .actual(Amount::usd_microcents(3200))
        .build()
    ).await?;
}
# Ok(())
# }
```

## Error Handling

Errors use pattern matching:

```rust,no_run
use runcycles::Error;

# fn example(err: Error) {
match err {
    Error::BudgetExceeded { message, .. } => {
        println!("Budget exceeded: {}", message);
    }
    Error::Api { status, code, .. } => {
        println!("API error ({}): {:?}", status, code);
    }
    Error::Transport(e) => {
        println!("Network error: {}", e);
    }
    _ => {}
}
# }
```

## Configuration

### From code

```rust,no_run
# use runcycles::CyclesClient;
let client = CyclesClient::builder("my-api-key", "http://localhost:7878")
    .tenant("acme")
    .workspace("production")
    .connect_timeout(std::time::Duration::from_secs(2))
    .read_timeout(std::time::Duration::from_secs(5))
    .retry_enabled(true)
    .retry_max_attempts(5)
    .build();
```

### From environment

```rust,no_run
# use runcycles::{CyclesClient, CyclesConfig};
// Reads CYCLES_BASE_URL, CYCLES_API_KEY, CYCLES_TENANT, etc.
let config = CyclesConfig::from_env().expect("missing env vars");
let client = CyclesClient::new(config);
```

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `rustls-tls` | Yes | Use rustls for TLS |
| `native-tls` | No | Use platform-native TLS |
| `blocking` | No | Synchronous blocking client |

## License

Apache-2.0
