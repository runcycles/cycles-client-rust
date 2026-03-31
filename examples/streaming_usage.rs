//! Streaming usage with the RAII guard.
//!
//! Demonstrates using the guard for LLM streaming, where the actual cost
//! is only known after the stream completes.

use runcycles::models::*;
use runcycles::CyclesClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = CyclesClient::builder("my-api-key", "http://localhost:7878")
        .tenant("acme")
        .build();

    // Reserve budget before starting the stream
    let guard = client
        .reserve(
            ReservationCreateRequest::builder()
                .subject(Subject {
                    tenant: Some("acme".into()),
                    ..Default::default()
                })
                .action(Action::new("llm.completion", "gpt-4o"))
                .estimate(Amount::tokens(1000))
                .ttl_ms(30_000_u64)
                .build(),
        )
        .await?;

    // The heartbeat automatically extends TTL in the background.
    // Start your streaming operation here...
    // let stream = start_llm_stream().await;

    // Simulate consuming a stream and accumulating token counts
    let mut total_input_tokens = 0i64;
    let mut total_output_tokens = 0i64;
    for _chunk in 0..10 {
        // Process each chunk...
        total_input_tokens += 50;
        total_output_tokens += 100;
    }

    // Stream complete — commit with actual usage
    guard
        .commit(
            CommitRequest::builder()
                .actual(Amount::tokens(total_input_tokens + total_output_tokens))
                .metrics(CyclesMetrics {
                    tokens_input: Some(total_input_tokens),
                    tokens_output: Some(total_output_tokens),
                    ..Default::default()
                })
                .build(),
        )
        .await?;

    println!(
        "Streamed: {} input + {} output tokens",
        total_input_tokens, total_output_tokens
    );

    Ok(())
}
