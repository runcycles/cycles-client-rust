[![Crates.io](https://img.shields.io/crates/v/runcycles)](https://crates.io/crates/runcycles)
[![docs.rs](https://img.shields.io/docsrs/runcycles)](https://docs.rs/runcycles)
[![CI](https://github.com/runcycles/cycles-client-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/runcycles/cycles-client-rust/actions)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue)](LICENSE)
[![Coverage](https://img.shields.io/badge/coverage-95%25-brightgreen)](https://github.com/runcycles/cycles-client-rust/actions)

# Cycles Rust Client — Runtime Authority for AI Agents (Spend, Actions, Audit)

Tokio-native Rust client for the [Cycles](https://runcycles.io) protocol — runtime authority over autonomous AI agents. Cycles enforces hard limits on three things observation alone can't fix:

- **Spend** — reserve-commit budget enforcement that stops runaway LLM cost *before* the next call, not after the invoice arrives.
- **Risky actions** — three-way decisions (`Allow` / `AllowWithCaps` / `Deny`) with caps for tool denylists, max tokens, max steps, and cooldowns. The client enforces caps before the agent acts.
- **Audit gaps** — every reservation, commit, release, and decision is a signed event. Compliance, incident review, and per-agent attribution come for free, not as a separate logging project.

This crate implements the reserve-execute-commit lifecycle with an idiomatic Rust API built around RAII guards, ownership semantics, and `Send + Sync` concurrency. Same wire protocol as the Python, TypeScript, and Spring Boot clients — switch languages without changing the server.

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

> **Want a real LLM example?** [`examples/async_openai_completion.rs`](examples/async_openai_completion.rs) wires the same `with_cycles` flow against `async-openai`, threading the response's `usage.total_tokens` back into the commit. Run it with `cargo run --example async_openai_completion` — requires `OPENAI_API_KEY` in the env plus a reachable Cycles server, a tenant API key, and a `TOKENS`-denominated budget at the scope the example reserves against.

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
    .journal_enabled(true)
    .build();
```

### From environment

```rust,no_run
# use runcycles::{CyclesClient, CyclesConfig};
// Reads CYCLES_BASE_URL, CYCLES_API_KEY, CYCLES_TENANT, etc.
let config = CyclesConfig::from_env().expect("missing env vars");
let client = CyclesClient::new(config);
```

### Commit retry

If the attempt inside `guard.commit(...)` fails with a retryable error — a
transport failure, a 5xx server error, or an error code the protocol
classifies as transient (e.g. `LIMIT_EXCEEDED` rate limiting; unrecognized
error codes are treated as transient for forward compatibility, see
`Error::is_retryable`) — and `retry_enabled` is set (the default), the commit
is retried **inline** with exponential backoff (`retry_initial_delay` ×
`retry_multiplier`, capped at `retry_max_delay`, up to `retry_max_attempts`
attempts). On HTTP 429 the retry waits at least the server's `Retry-After`
delay, even when that exceeds `retry_max_delay`. Retries reuse the original
request — same idempotency key — so a commit that already landed server-side
cannot double-charge, and the reservation heartbeat keeps extending the TTL
while inline retries run.

Actual spend is also written atomically to a durable journal before the first
commit request. Only a schema-valid HTTP 200 `COMMITTED` response (or a
schema-valid HTTP 201 `APPLIED` event fallback) proves settlement success;
other 2xx outcomes remain ambiguous. A proven success or understood terminal
rejection removes the record. Retry exhaustion, authentication failure, and an
ambiguous client response retain it for next-run replay. In that case `commit()` returns
`Error::CommitPending`; do not compensate with a different idempotency key.
Clients created inside Tokio automatically start one replay pass, and
`flush_pending_commits_with_timeout(...)` provides a bounded graceful-shutdown
drain. The blocking client exposes the same operation.

The journal defaults to `~/.runcycles/commit-journal`, partitioned by server
and principal. A configured tenant is the principal, so rotating its API key
does not orphan pending records; API keys are never written to records. Set
`journal_enabled(false)` only if the application supplies equivalent
durability. Configuration is also available through `CYCLES_JOURNAL_ENABLED`
and `CYCLES_JOURNAL_DIR`. Retry env knobs remain
`CYCLES_RETRY_ENABLED`, `CYCLES_RETRY_MAX_ATTEMPTS`,
`CYCLES_RETRY_INITIAL_DELAY`, `CYCLES_RETRY_MULTIPLIER`, and
`CYCLES_RETRY_MAX_DELAY`.

The durable journal belongs to the high-level `ReservationGuard::commit`
lifecycle. Low-level `commit_reservation` and `create_event` calls enforce the
same response predicates and expose the original idempotent primitives, but do
not automatically persist application-owned requests. Callers using those
primitives directly must provide equivalent durable retry storage if they need
restart convergence.

### Expired-commit event fallback

A commit records spend that **already happened**, so a `RESERVATION_EXPIRED`
rejection (the commit landed after the reservation's grace period; the
server already returned the reserved budget to the pool) must not silently
drop the spend. When the commit path receives `RESERVATION_EXPIRED`,
`guard.commit(...)` records the spend as a post-hoc direct-debit event
(`POST /v1/events`, no reservation needed) instead, reusing the commit's
idempotency key (exactly-once) and marking the event's metadata with
`recovered_reservation_id` and
`recovery_reason = "commit_after_reservation_expired"`. Transient event
failures are retried with the same backoff policy.

On fallback success, `commit()` returns `Ok` with
`CommitStatus::RecoveredViaEvent` and `CommitResponse::recovered_via_event`
set to the recorded event's ID. The journal switches to event mode before the
fallback attempt, so a restart never retries an already-expired reservation.
An unresolved fallback remains journaled and returns `Error::CommitPending`.
With journaling disabled, fallback failure returns
`Error::CommitRecoveryFailed` carrying both underlying errors.

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `rustls-tls` | Yes | Use rustls for TLS |
| `native-tls` | No | Use platform-native TLS |
| `blocking` | No | Synchronous blocking client |

## License

Apache-2.0
