# runcycles

Rust client for the [Cycles](https://runcycles.io) budget authority protocol.

Cycles provides concurrency-safe spend and action control for autonomous agent
runtimes. This crate implements the reserve-execute-commit lifecycle with an
idiomatic Rust API built around RAII guards and ownership semantics.

## Installation

```toml
[dependencies]
runcycles = "0.1"
```

## Quick Start

```rust,no_run
use runcycles::{CyclesClient, Error, models::*};

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Create a client
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

### RAII Guard (Primary API)

The `ReservationGuard` is the core abstraction. It holds a live reservation and
auto-extends TTL via a background heartbeat. The guard IS the context — no
thread-locals or task-locals needed.

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
