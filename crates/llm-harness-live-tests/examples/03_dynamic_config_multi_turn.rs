//! 03 — Dynamic Config & Multi-Turn: change settings at runtime and
//! continue a conversation across multiple turns.
//!
//! Demonstrates:
//! - set_system_prompt: change the system prompt between turns
//! - set_temperature: change sampling temperature
//! - prompt(): each call continues the session (history preserved)
//! - Usage tracking across turns
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 03_dynamic_config_multi_turn

use llm_harness_live_tests::{EventCapture, LiveProvider, MULTI_TURN, make_harness, with_timeout};

/// Run a prompt turn: send a message, wait for completion, print the response.
async fn prompt_turn(harness: &llm_harness_agent::AgentHarness, label: &str, message: &str) {
    println!("--- {} ---", label);
    let capture = EventCapture::subscribe(harness);
    with_timeout(MULTI_TURN, harness.prompt(message))
        .await
        .expect("prompt failed");
    capture.settle().await;
    println!("Response: {}\n", capture.texts());
}

#[tokio::main]
async fn main() {
    println!("=== 03: Dynamic Config & Multi-Turn ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .expect("set OPENAI_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    let (harness, _inspector, _tmp) = make_harness(entry).await;

    // ── Turn 1: Establish two facts to remember ─────────────────────────────
    prompt_turn(
        &harness,
        "Turn 1: Establish facts",
        "My name is Alice and my favorite number is 42. Remember both for this conversation.",
    )
    .await;

    // ── Turn 2: Verify name recall ──────────────────────────────────────────
    prompt_turn(&harness, "Turn 2: Verify name recall", "What is my name?").await;

    // ── Turn 3: Verify number recall ────────────────────────────────────────
    prompt_turn(
        &harness,
        "Turn 3: Verify favorite number recall",
        "What is my favorite number?",
    )
    .await;

    // ── Change system prompt to pirate mode ─────────────────────────────────
    println!("--- Changing system prompt to pirate mode ---\n");
    harness.set_system_prompt(Some(
        "You are a pirate. Always speak like a pirate. Use 'Arrr' and nautical terms.".to_string(),
    ));

    // ── Turn 4: Pirate mode + recall both facts ─────────────────────────────
    prompt_turn(
        &harness,
        "Turn 4: Pirate mode + recall both facts",
        "Tell me both my name and my favorite number. Answer in character.",
    )
    .await;

    // ── Change temperature to 0.7 ───────────────────────────────────────────
    println!("--- Changing temperature to 0.7 (creative) ---\n");
    harness
        .set_temperature(Some(0.7))
        .await
        .expect("set_temperature failed");

    // ── Turn 5: Creative temperature + pirate still active ──────────────────
    prompt_turn(
        &harness,
        "Turn 5: Creative temperature + pirate still active",
        "Ask me a question about my favorite number, in character.",
    )
    .await;

    // ── Summary ─────────────────────────────────────────────────────────────
    println!("--- Summary ---");
    let usage = harness.usage();
    println!(
        "Total tokens: {} in / {} out",
        usage.total_input_tokens, usage.total_output_tokens
    );
}
