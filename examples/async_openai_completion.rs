//! Wrap a real OpenAI chat completion call with Cycles reserve-commit.
//!
//! This is the example most users actually want: instead of the `call_llm`
//! placeholder in `with_cycles_usage.rs`, this drives an actual `async-openai`
//! ChatCompletion request and feeds the response's `usage.total_tokens` back
//! into the Cycles commit so the budget reflects real spend.
//!
//! Requirements to run:
//!
//!   - A running Cycles server reachable at the URL passed to
//!     `CyclesClient::builder` (or override via `CYCLES_BASE_URL` env var).
//!   - `OPENAI_API_KEY` set in the environment (async-openai's `Client::new`
//!     reads it from there).
//!
//! Run:
//!
//!   cargo run --example async_openai_completion
//!
//! For streaming chat completions, the lifecycle uses `ReservationGuard`
//! instead of `with_cycles` because token totals are only known after the
//! stream ends. See the docs site for the streaming pattern.

use async_openai::{
    types::{ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequestArgs},
    Client,
};
use runcycles::models::*;
use runcycles::{with_cycles, CyclesClient, WithCyclesConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cycles = CyclesClient::builder("my-api-key", "http://localhost:7878")
        .tenant("acme")
        .build();

    // Reads OPENAI_API_KEY from the environment.
    let openai = Client::new();

    let prompt = "Summarize the runcycles crate in one sentence.";

    let reply = with_cycles(
        &cycles,
        WithCyclesConfig::new(Amount::tokens(1_500))
            .action("llm.completion", "gpt-4o-mini")
            .subject(Subject {
                tenant: Some("acme".into()),
                ..Default::default()
            }),
        |ctx| async move {
            // If the server returned ALLOW_WITH_CAPS, narrow max_tokens.
            let mut max_tokens: u32 = 800;
            if let Some(caps) = &ctx.caps {
                if let Some(cap) = caps.max_tokens {
                    // caps.max_tokens is i64; clamp into the OpenAI u32 range.
                    let cap_u32 = u32::try_from(cap.max(0)).unwrap_or(max_tokens);
                    max_tokens = cap_u32.min(max_tokens);
                }
            }

            let request = CreateChatCompletionRequestArgs::default()
                .model("gpt-4o-mini")
                .max_tokens(max_tokens)
                .messages([ChatCompletionRequestUserMessageArgs::default()
                    .content(prompt)
                    .build()?
                    .into()])
                .build()?;

            let response = openai.chat().create(request).await?;

            let text = response
                .choices
                .first()
                .and_then(|c| c.message.content.clone())
                .unwrap_or_default();

            // The unit passed to with_cycles is TOKENS, so the actual must be
            // tokens too. If usage is absent (some OpenAI-compatible providers
            // omit it), treat as zero rather than over-charging.
            let actual_tokens = response.usage.map(|u| u.total_tokens as i64).unwrap_or(0);

            Ok((text, Amount::tokens(actual_tokens)))
        },
    )
    .await?;

    println!("Reply: {reply}");
    Ok(())
}
