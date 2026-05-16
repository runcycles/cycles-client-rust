//! Wrap a real OpenAI chat completion call with Cycles reserve-commit.
//!
//! Instead of the `call_llm` placeholder in `with_cycles_usage.rs`, this drives
//! an actual `async-openai` ChatCompletion request and feeds the response's
//! `usage.total_tokens` back into the Cycles commit so the budget reflects
//! real spend.
//!
//! ## Loud-failure stance
//!
//! This example errors out instead of swallowing edge cases that would lead to
//! silent under-billing — a missing `usage` field, an empty `choices` array,
//! or a `caps.max_tokens` value of 0 all return `Err` from the closure, which
//! causes `with_cycles` to release the reservation rather than commit a wrong
//! amount. Production code that needs a fallback (e.g. commit the reservation
//! estimate when the provider omits `usage`) should make that choice
//! explicitly; the default should not be "commit zero."
//!
//! ## Requirements to run
//!
//!   - A running Cycles server at the URL passed to `CyclesClient::builder`
//!     (the example hardcodes `http://localhost:7878`), with a tenant `acme`
//!     and a `TOKENS`-denominated budget that covers the reservation.
//!   - A Cycles API key matching that tenant (the example hardcodes
//!     `"my-api-key"`).
//!   - `OPENAI_API_KEY` set in the environment (async-openai's `Client::new`
//!     reads it from there).
//!
//! ## Run
//!
//!   cargo run --example async_openai_completion
//!
//! For streaming chat completions, the right primitive is `ReservationGuard`
//! rather than `with_cycles`, because the closure has to return both the
//! value and the actual cost in one shot — and a streamed response's total
//! token count is only known after the stream ends. See `streaming_usage.rs`
//! for the guard-based pattern.

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
            // Narrow max_tokens against the server's cap, if one was returned.
            // A non-positive cap is a hard error — sending max_tokens=0 to
            // OpenAI would request zero output, which is never what the caller
            // wanted, and it would still consume the request budget.
            let mut max_tokens: u32 = 800;
            if let Some(caps) = &ctx.caps {
                if let Some(cap) = caps.max_tokens {
                    let cap_u32 = u32::try_from(cap)
                        .map_err(|_| "caps.max_tokens is negative — refusing to call OpenAI")?;
                    if cap_u32 == 0 {
                        return Err("caps.max_tokens is 0 — refusing to call OpenAI".into());
                    }
                    max_tokens = cap_u32.min(max_tokens);
                }
            }

            let request = CreateChatCompletionRequestArgs::default()
                .model("gpt-4o-mini")
                // max_completion_tokens is the current field name; `max_tokens`
                // is deprecated upstream for chat completions.
                .max_completion_tokens(max_tokens)
                .messages([ChatCompletionRequestUserMessageArgs::default()
                    .content(prompt)
                    .build()?
                    .into()])
                .build()?;

            let response = openai.chat().create(request).await?;

            // Choices empty / content missing → fail the closure so the
            // reservation auto-releases instead of committing zero against
            // a successful-looking response.
            let text = response
                .choices
                .first()
                .and_then(|c| c.message.content.clone())
                .ok_or("OpenAI response had no message content")?;

            // The unit passed to with_cycles is TOKENS, so the actual must be
            // tokens too. We require `usage` to be present — committing zero
            // on a missing-usage response silently under-bills the budget.
            // OpenAI-compatible providers that omit usage (some local/proxy
            // setups) should be wrapped with an explicit fallback in calling
            // code; the example refuses to guess.
            let usage = response
                .usage
                .ok_or("OpenAI response omitted usage — refusing to commit a guessed amount")?;
            let actual_tokens = i64::from(usage.total_tokens);

            Ok((text, Amount::tokens(actual_tokens)))
        },
    )
    .await?;

    println!("Reply: {reply}");
    Ok(())
}
