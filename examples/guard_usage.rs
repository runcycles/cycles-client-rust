//! RAII guard usage.
//!
//! Demonstrates using the `ReservationGuard` for automatic lifecycle
//! management, including caps checking and commit-by-ownership.

use runcycles::models::*;
use runcycles::{CyclesClient, Error};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = CyclesClient::builder("my-api-key", "http://localhost:7878")
        .tenant("acme")
        .build();

    // Reserve returns an RAII guard
    let guard = match client
        .reserve(
            ReservationCreateRequest::builder()
                .subject(Subject {
                    tenant: Some("acme".into()),
                    ..Default::default()
                })
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::usd_microcents(5000))
                .build(),
        )
        .await
    {
        Ok(guard) => guard,
        Err(Error::BudgetExceeded { message, .. }) => {
            println!("Budget exceeded: {message}");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    println!("Reserved: {}", guard.reservation_id());
    println!("Decision: {:?}", guard.decision());

    // Check caps for soft constraints
    if guard.is_capped() {
        if let Some(caps) = guard.caps() {
            println!("Max tokens: {:?}", caps.max_tokens);
            if !caps.is_tool_allowed("web_search") {
                println!("web_search is not allowed under caps");
            }
        }
    }

    // Simulate work...

    // Commit consumes the guard (compile-time double-commit prevention)
    guard
        .commit(
            CommitRequest::builder()
                .actual(Amount::usd_microcents(3200))
                .metrics(CyclesMetrics {
                    tokens_input: Some(100),
                    tokens_output: Some(200),
                    ..Default::default()
                })
                .build(),
        )
        .await?;

    // guard.commit(...) here would be a COMPILE ERROR

    Ok(())
}
