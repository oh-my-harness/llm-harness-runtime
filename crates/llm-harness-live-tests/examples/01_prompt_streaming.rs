//! 01 — Prompt & Streaming: send a prompt and observe token-by-token streaming.
//!
//! Demonstrates:
//! - Constructing a real provider from environment variables
//! - Building a harness with HarnessBuilder
//! - Subscribing to events for real-time streaming output
//! - Collecting the full response text
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 01_prompt_streaming

use llm_harness_live_tests::{EventCapture, LiveProvider, MULTI_TURN, make_harness, with_timeout};

#[tokio::main]
async fn main() {
    println!("=== 01: Prompt & Streaming ===\n");

    // 1. Load provider from environment.
    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .expect("set OPENAI_API_KEY to run this example");

    println!("Provider: {} | Model: {}", entry.name, entry.model);

    // 2. Build harness.
    let (harness, _inspector, _tmp) = make_harness(entry).await;
    println!("Harness built. Working dir: {:?}\n", _tmp.path());

    // 3. Subscribe to events for streaming.
    let capture = EventCapture::subscribe(&harness);

    // 4. Send prompt — this blocks until the run completes.
    let prompt = "Explain what a closure is in one sentence.";
    println!("Prompt: \"{}\"\n", prompt);
    println!("Streaming response:\n");

    with_timeout(MULTI_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");

    capture.settle().await;

    // 5. Print the full response.
    let text = capture.texts();
    println!("\n\n--- Full response ---\n{}\n", text);

    // 6. Print token usage.
    let usage = harness.usage();
    println!(
        "Tokens: {} in / {} out",
        usage.total_input_tokens, usage.total_output_tokens
    );
}
