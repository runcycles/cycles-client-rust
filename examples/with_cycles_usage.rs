//! Automatic lifecycle with `with_cycles` — like Python's `@cycles` decorator.
//!
//! This is the simplest way to integrate Cycles into existing LLM calls.
//! Reserve, execute, and commit/release are handled automatically.
//!
//! **Unit must match the budget.** The `Amount` unit passed to
//! `WithCyclesConfig::new` (and the `actual` returned by the closure) must
//! match the unit of the active budget at the target scope. Budgets are
//! indexed by `(scope, unit)` server-side, so a mismatched unit surfaces as a
//! 404 "Budget not found for provided scope: …" — even when the scope exists.

use runcycles::models::*;
use runcycles::{with_cycles, CyclesClient, WithCyclesConfig};

/// Simulate an LLM call.
async fn call_llm(prompt: &str, max_tokens: i64) -> (String, i64, i64) {
    let reply = format!("Response to: {prompt}");
    let input_tokens = prompt.len() as i64;
    let output_tokens = reply.len() as i64;
    let _ = max_tokens;
    (reply, input_tokens, output_tokens)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = CyclesClient::builder("my-api-key", "http://localhost:7878")
        .tenant("acme")
        .build();

    // ─── Simple: fixed estimate ──────────────────────────────────
    let reply = with_cycles(
        &client,
        WithCyclesConfig::new(Amount::tokens(1000))
            .action("llm.completion", "gpt-4o")
            .subject(Subject {
                tenant: Some("acme".into()),
                ..Default::default()
            }),
        |_ctx| async move {
            let (reply, _inp, out) = call_llm("Hello", 1000).await;
            Ok((reply, Amount::tokens(out)))
        },
    )
    .await?;

    println!("Simple: {reply}");

    // ─── With caps: respect budget constraints ───────────────────
    let reply = with_cycles(
        &client,
        WithCyclesConfig::new(Amount::tokens(2000))
            .action("llm.completion", "gpt-4o")
            .subject(Subject {
                tenant: Some("acme".into()),
                ..Default::default()
            }),
        |ctx| async move {
            // If server returns caps, respect max_tokens
            let max_tokens = ctx.caps.as_ref().and_then(|c| c.max_tokens).unwrap_or(2000);

            let (reply, inp, out) = call_llm("Write a poem", max_tokens).await;
            println!("Used {inp} input + {out} output tokens");
            Ok((reply, Amount::tokens(inp + out)))
        },
    )
    .await?;

    println!("With caps: {reply}");

    // ─── With metrics: attach observability data ─────────────────
    let reply = with_cycles(
        &client,
        WithCyclesConfig::new(Amount::usd_microcents(50000))
            .action("llm.completion", "gpt-4o")
            .subject(Subject {
                tenant: Some("acme".into()),
                ..Default::default()
            })
            .metrics(CyclesMetrics {
                model_version: Some("gpt-4o-2024-05".into()),
                ..Default::default()
            }),
        |_ctx| async move {
            let (reply, _inp, _out) = call_llm("Explain Rust", 500).await;
            Ok((reply, Amount::usd_microcents(32000)))
        },
    )
    .await?;

    println!("With metrics: {reply}");

    Ok(())
}
