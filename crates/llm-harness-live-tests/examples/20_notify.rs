//! 20 — NotifyPlugin: LLM-callable user notifications.
//!
//! Demonstrates:
//! - Part 1: NotifyPlugin + ConsoleChannel — the LLM calls `notify_user`
//!   to send a single notification; delivery is observed on the channel.
//! - Part 2: Multiple notifications with different urgency levels — the LLM
//!   calls `notify_user` several times; all deliveries are observed.
//!
//! `NotifyPlugin` only registers the `notify_user` tool. The actual delivery
//! channel is injected **per run** via
//! `RunRequest::with_extension(Arc<dyn NotificationChannel>)`. This keeps the
//! channel out of the builder — different runs can use different channels
//! (IM, email, console, …) without rebuilding the harness.
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 20_notify

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness_with, with_timeout,
};
use llm_harness_strategy::{
    NotificationChannel, NotificationError, NotificationMessage, NotifyPlugin, Urgency,
};
use llm_harness_types::RunRequest;
use tokio::sync::Mutex;

// ── ConsoleChannel ───────────────────────────────────────────────────────────

/// A notification channel that prints to stdout and records every message it
/// receives, so the example can assert delivery after the run completes.
struct ConsoleChannel {
    received: Arc<Mutex<Vec<NotificationMessage>>>,
}

impl ConsoleChannel {
    fn new(received: Arc<Mutex<Vec<NotificationMessage>>>) -> Self {
        Self { received }
    }
}

impl NotificationChannel for ConsoleChannel {
    fn send(&self, msg: NotificationMessage) -> BoxFuture<'_, Result<(), NotificationError>> {
        let received = self.received.clone();
        Box::pin(async move {
            println!("[notification] (urgency={:?}) {}", msg.urgency, msg.text);
            received.lock().await.push(msg);
            Ok(())
        })
    }
}

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    println!("=== 20: NotifyPlugin (user notifications) ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .or(provider.anthropic.as_ref())
        .expect("set OPENAI_API_KEY or ANTHROPIC_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    part1_single_notification(entry).await;
    println!();
    part2_multiple_urgency(entry).await;

    println!("\n--- Summary ---");
    println!("Part 1: notify_user tool called, channel received the message");
    println!("Part 2: multiple notify_user calls with low/normal/high urgency delivered");
}

// ── Part 1: single notification ──────────────────────────────────────────────

async fn part1_single_notification(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 1: NotifyPlugin + ConsoleChannel (single notification) ---\n");

    // The plugin only registers the tool; the channel is per-run.
    let (harness, _inspector, _tmp) =
        make_harness_with(entry, |b| b.install(&NotifyPlugin::new())).await;

    let cap = EventCapture::subscribe(&harness);

    let received: Arc<Mutex<Vec<NotificationMessage>>> = Arc::new(Mutex::new(Vec::new()));
    let channel = Arc::new(ConsoleChannel::new(received.clone())) as Arc<dyn NotificationChannel>;

    let prompt_text =
        "Use the notify_user tool to send a notification saying 'Task completed successfully'.";
    let request = RunRequest::from_text(prompt_text).with_extension(channel);

    println!("LLM prompt: {}", prompt_text);
    with_timeout(SINGLE_TURN, harness.run(request))
        .await
        .expect("run failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    let delivered = received.lock().await.clone();

    println!("\nTools called: {:?}", tools_called);
    println!("Notifications delivered to channel: {}", delivered.len());
    for msg in &delivered {
        println!("  - (urgency={:?}) {}", msg.urgency, msg.text);
    }
    println!(
        "Final text:   {}",
        cap.texts().chars().take(160).collect::<String>()
    );

    let notify_called = tools_called.iter().any(|t| t == "notify_user");
    println!("\nObservation:");
    println!("  notify_user tool called: {}", notify_called);
    println!("  channel received message: {}", !delivered.is_empty());
    if !delivered.is_empty() {
        println!("  delivered text: {:?}", delivered[0].text);
    }
}

// ── Part 2: multiple notifications with urgency levels ───────────────────────

async fn part2_multiple_urgency(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 2: multiple notifications with urgency levels ---\n");

    let (harness, _inspector, _tmp) =
        make_harness_with(entry, |b| b.install(&NotifyPlugin::new())).await;

    let cap = EventCapture::subscribe(&harness);

    let received: Arc<Mutex<Vec<NotificationMessage>>> = Arc::new(Mutex::new(Vec::new()));
    let channel = Arc::new(ConsoleChannel::new(received.clone())) as Arc<dyn NotificationChannel>;

    let prompt_text = "Send three separate notifications using the notify_user tool, \
                       each with a different urgency level (low, normal, and high). \
                       For example: 'Low battery warning' (low), 'Build finished' (normal), \
                       and 'Critical security alert' (high). Call the tool three times.";
    let request = RunRequest::from_text(prompt_text).with_extension(channel);

    println!("LLM prompt: {}", prompt_text);
    with_timeout(SINGLE_TURN, harness.run(request))
        .await
        .expect("run failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    let delivered = received.lock().await.clone();

    let notify_call_count = tools_called.iter().filter(|t| *t == "notify_user").count();

    println!("\nTools called: {:?}", tools_called);
    println!("notify_user call count: {}", notify_call_count);
    println!("Notifications delivered to channel: {}", delivered.len());
    for (i, msg) in delivered.iter().enumerate() {
        println!("  {}. (urgency={:?}) {}", i + 1, msg.urgency, msg.text);
    }
    println!(
        "Final text:   {}",
        cap.texts().chars().take(160).collect::<String>()
    );

    let urgencies: Vec<Urgency> = delivered.iter().map(|m| m.urgency).collect();
    println!("\nObservation:");
    println!("  notify_user called {} time(s)", notify_call_count);
    println!("  channel received {} message(s)", delivered.len());
    println!("  urgency values seen: {:?}", urgencies);
}
