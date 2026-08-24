//! 25 — Inspector Interactive: human-in-the-loop via the Web API.
//!
//! Demonstrates the Inspector's interactive endpoints:
//! - Mount Inspector on a port (reads `SENZA_INSPECT` env var or defaults to 8080)
//! - Start a task via `harness.prompt()` (or `POST /api/prompt` from the browser)
//! - While the agent works, the user can interact via the Web UI:
//!   - `POST /api/inject` — inject a steering message (`harness.steer()`)
//!   - `POST /api/abort`  — cancel the current turn (`harness.abort()`)
//!   - `GET /api/agent/usage`  — token/cost snapshot
//!   - `GET /api/agent/skills` — skill list
//! - WebSocket `/api/events` streams all agent events live in the browser
//!
//! The script starts a task, then keeps the Inspector alive for 5 minutes
//! (or until Ctrl+C) so you can open the browser and interact.
//!
//! Run:
//!   source .env
//!   SENZA_INSPECT=8080 cargo run -p llm-harness-live-tests --example 25_inspector_interact

use llm_harness_live_tests::{LiveProvider, make_harness};

#[tokio::main]
async fn main() {
    println!("=== 25: Inspector Interactive (human-in-the-loop via Web UI) ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .expect("set OPENAI_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    let (harness, _inspector, _tmp) = make_harness(entry).await;

    // Inspector port — from env var SENZA_INSPECT, default 8080.
    let port = std::env::var("SENZA_INSPECT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let url = format!("http://127.0.0.1:{port}");

    println!("Inspector running at {url}");
    println!("  Open this URL in your browser to interact with the agent.");
    println!();
    println!("Available endpoints:");
    println!("  GET  {url}/                    — Web UI (event stream + controls)");
    println!("  GET  {url}/api/agent/config    — Agent configuration");
    println!("  GET  {url}/api/agent/session   — Session history");
    println!("  GET  {url}/api/agent/usage     — Token/cost usage");
    println!("  GET  {url}/api/agent/skills    — Skill list");
    println!("  GET  {url}/api/events          — WebSocket event stream");
    println!("  POST {url}/api/inject          — Inject message: {{\"text\": \"...\"}}");
    println!("  POST {url}/api/abort           — Abort current turn");
    println!("  POST {url}/api/prompt          — Start new task: {{\"text\": \"...\"}}");
    println!();
    println!("Try this flow:");
    println!("  1. The initial task starts automatically below.");
    println!("  2. While the agent works, type a steering message in the browser and click Send.");
    println!("  3. Click Abort to cancel if it goes off track.");
    println!("  4. When the agent settles, type a new task in the browser and click Start.");
    println!();
    println!("Press Ctrl+C to stop early.");
    println!("{}", "─".repeat(60));

    // ── Wait for user interaction via browser ───────────────────────────
    // The user starts tasks by typing in the Inspector Web UI's
    // "Start new task" input box (POST /api/prompt).
    // We just keep the process alive for 5 minutes.
    println!();
    println!("Waiting for tasks from the browser...");
    println!("Type a task in the Inspector Web UI and click Start.");
    println!();

    use std::time::Duration;
    tokio::time::sleep(Duration::from_secs(300)).await;

    // ── Show final usage ───────────────────────────────────────────────
    let usage = harness.usage();
    println!("\n--- Final Usage ---");
    println!("  Input tokens:  {}", usage.total_input_tokens);
    println!("  Output tokens: {}", usage.total_output_tokens);
    println!("  Cost:          ${:.4}", usage.total_cost);

    println!("\nShutting down...");
    drop(_inspector);
    println!("Done.");
}
