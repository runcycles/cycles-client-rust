//! Error handling patterns.
//!
//! Demonstrates how to match on different error types and handle
//! budget exceeded, retryable errors, and transport failures.

use runcycles::models::*;
use runcycles::{CyclesClient, Error};

#[tokio::main]
async fn main() {
    let client = CyclesClient::builder("my-api-key", "http://localhost:7878")
        .tenant("acme")
        .build();

    let result = client
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
        .await;

    match result {
        Ok(guard) => {
            println!("Reserved: {}", guard.reservation_id());
            // In production, you would do work here and then commit.
            // For this example, just release.
            let _ = guard.release("example_done").await;
        }
        Err(Error::BudgetExceeded {
            message,
            retry_after,
            ..
        }) => {
            println!("Budget exceeded: {message}");
            if let Some(delay) = retry_after {
                println!("Retry after: {:?}", delay);
            }
        }
        Err(Error::Api {
            status,
            code,
            message,
            ..
        }) => {
            println!("API error ({status}): {message}");
            if let Some(code) = code {
                println!("Error code: {code:?}");
            }
        }
        Err(Error::Transport(e)) => {
            println!("Network error: {e}");
            println!("Is retryable: true");
        }
        Err(e) => {
            println!("Other error: {e}");
            println!("Retryable: {}", e.is_retryable());
        }
    }
}
