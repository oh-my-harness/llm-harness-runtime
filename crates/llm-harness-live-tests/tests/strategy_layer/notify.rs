//! Tests for the NotifyPlugin: the `notify_user` tool is registered and
//! callable by the LLM, and a `NotificationChannel` injected via `RunRequest`
//! receives the message.
//!
//! Spec §7: NotifyUserTool is called; NotificationChannel receives the message.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_live_tests::skip_if_none;
use llm_harness_live_tests::{
    CapturedEvent, EventCapture, LiveProvider, SINGLE_TURN, assert_event_emitted,
    assert_tool_was_called, make_harness_with, with_timeout,
};
use llm_harness_strategy::{
    NotificationChannel, NotificationError, NotificationMessage, NotifyPlugin,
};
use llm_harness_types::RunRequest;

// ── Recording channel ───────────────────────────────────────────────────

/// A `NotificationChannel` that stores every received message in a shared
/// vector so tests can assert on delivery after the run completes.
struct RecordingChannel {
    messages: Arc<Mutex<Vec<String>>>,
}

impl RecordingChannel {
    fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let ch = Self {
            messages: messages.clone(),
        };
        (ch, messages)
    }
}

impl NotificationChannel for RecordingChannel {
    fn send(&self, msg: NotificationMessage) -> BoxFuture<'_, Result<(), NotificationError>> {
        self.messages.lock().unwrap().push(msg.text);
        Box::pin(async { Ok(()) })
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

/// The `notify_user` tool registered by `NotifyPlugin` is callable by the
/// LLM: a prompt asking the model to send a notification results in a
/// `notify_user` tool call and the conversation settles normally.
#[tokio::test]
async fn notify_user_tool_called() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let (harness, _tmp) = make_harness_with(entry, |b| b.install(&NotifyPlugin::new())).await;
    let cap = EventCapture::subscribe(&harness);

    let (channel, _messages) = RecordingChannel::new();
    let request = RunRequest::from_text(
        "Use the notify_user tool to send a notification saying 'Task done'.",
    )
    .with_extension(Arc::new(channel) as Arc<dyn NotificationChannel>);

    with_timeout(SINGLE_TURN, harness.run(request))
        .await
        .expect("run should complete successfully");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "notify_user");
    assert_event_emitted(&events, &CapturedEvent::Settled);
}

/// The `NotificationChannel` injected via `RunRequest::with_extension`
/// receives the message text produced by the LLM's `notify_user` call.
#[tokio::test]
async fn channel_receives_message() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let (harness, _tmp) = make_harness_with(entry, |b| b.install(&NotifyPlugin::new())).await;
    let cap = EventCapture::subscribe(&harness);

    let (channel, messages) = RecordingChannel::new();
    let request = RunRequest::from_text(
        "Use the notify_user tool to send a notification saying 'Task done'.",
    )
    .with_extension(Arc::new(channel) as Arc<dyn NotificationChannel>);

    with_timeout(SINGLE_TURN, harness.run(request))
        .await
        .expect("run should complete successfully");
    cap.settle().await;

    // The channel must have received at least one message containing the
    // expected substring (case-insensitive).
    let received: Vec<String> = messages.lock().unwrap().clone();
    assert!(
        !received.is_empty(),
        "RecordingChannel should have received at least one notification"
    );
    let combined = received.join("\n").to_lowercase();
    assert!(
        combined.contains("task done"),
        "channel message should contain 'Task done'; got: {combined}"
    );
}
