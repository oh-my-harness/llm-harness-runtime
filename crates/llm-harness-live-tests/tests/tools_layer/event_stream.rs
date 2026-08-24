//! Tests for EventStream implementations (Spec §7).
//!
//! Covers:
//! - `TimerStream::once` — one-shot timer fires after a delay
//! - `WebhookStream` — external payload pushed via `WebhookChannel`
//! - `HeartbeatStream` — watchdog fires when no `tick()` arrives
//! - `ShellMonitorStream` — captures stdout from a shell command
//!
//! Each stream is wrapped in `WaitForExternalEventTool`, registered with the
//! harness, and the LLM is asked to call `wait_for_external_event`. The tool
//! blocks on `stream.next().await`; when the event arrives the tool returns
//! and the conversation continues.

use std::sync::Arc;
use std::time::Duration;

use llm_harness_live_tests::skip_if_none;
use llm_harness_live_tests::{
    CapturedEvent, EventCapture, LiveProvider, MULTI_TURN, assert_event_emitted,
    assert_tool_was_called, make_harness_with, with_timeout,
};
use llm_harness_strategy::{
    HeartbeatStream, ShellMonitorConfig, ShellMonitorStream, TimerStream, WebhookStream,
};
use llm_harness_types::{EventStream, Tool};
use llm_harness_workflow::lifecycle::event::WaitForExternalEventTool;
use llm_harness_workflow::lifecycle::task::TaskId;
use llm_harness_workflow::lifecycle::task_store::JsonlTaskStore;
use tokio::sync::Mutex;

// ── Tests ───────────────────────────────────────────────────────────────

/// `TimerStream::once` fires at a scheduled time. The LLM calls
/// `wait_for_external_event`; the tool blocks until the timer fires (2s),
/// then returns the event. The conversation settles normally.
#[ignore = "GLM streaming bug: WaitForExternalEventTool has empty properties schema, LLM won't emit tool_call chunk"]
#[tokio::test]
async fn timer_stream_fires() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let task_dir = tempfile::tempdir().unwrap();
    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        let fire_at = chrono::Utc::now() + chrono::Duration::seconds(2);
        let stream: Box<dyn EventStream> = Box::new(TimerStream::once(fire_at, "test-timer"));
        let stream_arc = Arc::new(Mutex::new(stream));
        let task_store = Arc::new(JsonlTaskStore::new(task_dir.path().join("tasks")));
        let tool = WaitForExternalEventTool::new(
            stream_arc,
            serde_json::json!({"type": "timer", "label": "test-timer"}),
            Some(task_store),
            TaskId("task1".into()),
        );
        b.tool(Arc::new(tool) as Arc<dyn Tool>)
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        MULTI_TURN,
        harness.prompt(
            "Use the wait_for_external_event tool to wait for a scheduled timer event. \
             After the tool returns, tell me what happened.",
        ),
    )
    .await
    .expect("run should complete successfully");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "wait_for_external_event");
    assert_event_emitted(&events, &CapturedEvent::Settled);
}

/// `WebhookStream` receives an externally-pushed payload. A background task
/// pushes `{"event": "test"}` after 1s. The LLM calls
/// `wait_for_external_event`, the tool returns the webhook payload.
#[ignore = "GLM streaming bug: WaitForExternalEventTool has empty properties schema, LLM won't emit tool_call chunk"]
#[tokio::test]
async fn webhook_stream_receives_payload() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let task_dir = tempfile::tempdir().unwrap();
    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        let (channel, stream) = WebhookStream::new(16);
        let stream_arc = Arc::new(Mutex::new(stream));
        let task_store = Arc::new(JsonlTaskStore::new(task_dir.path().join("tasks")));

        // Spawn a delayed push so the webhook fires while the tool is waiting.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = channel.push(serde_json::json!({"event": "test"})).await;
        });

        let tool = WaitForExternalEventTool::new(
            stream_arc,
            serde_json::json!({"type": "webhook"}),
            Some(task_store),
            TaskId("task1".into()),
        );
        b.tool(Arc::new(tool) as Arc<dyn Tool>)
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        MULTI_TURN,
        harness.prompt(
            "Use the wait_for_external_event tool to wait for an incoming webhook event. \
             After the tool returns, tell me what payload you received.",
        ),
    )
    .await
    .expect("run should complete successfully");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "wait_for_external_event");
}

/// `HeartbeatStream` is an activity-driven watchdog. When `tick()` is never
/// called, the heartbeat fires after `timeout`. The LLM calls
/// `wait_for_external_event` and the tool returns the heartbeat event.
#[ignore = "GLM streaming bug: WaitForExternalEventTool has empty properties schema, LLM won't emit tool_call chunk"]
#[tokio::test]
async fn heartbeat_stream_times_out() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let task_dir = tempfile::tempdir().unwrap();
    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        let (_handle, stream) = HeartbeatStream::new(Duration::from_secs(1), "test-heartbeat");
        let stream_arc = Arc::new(Mutex::new(stream));
        let task_store = Arc::new(JsonlTaskStore::new(task_dir.path().join("tasks")));
        let tool = WaitForExternalEventTool::new(
            stream_arc,
            serde_json::json!({"type": "heartbeat", "label": "test-heartbeat"}),
            Some(task_store),
            TaskId("task1".into()),
        );
        b.tool(Arc::new(tool) as Arc<dyn Tool>)
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        MULTI_TURN,
        harness.prompt(
            "Use the wait_for_external_event tool to wait for a heartbeat event. \
             After the tool returns, tell me what happened.",
        ),
    )
    .await
    .expect("run should complete successfully");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "wait_for_external_event");
}

/// `ShellMonitorStream` spawns a shell command and emits its output. The LLM
/// calls `wait_for_external_event`; the tool returns the captured stdout
/// (`hello-from-shell`) once the command completes.
#[ignore = "GLM streaming bug: WaitForExternalEventTool has empty properties schema, LLM won't emit tool_call chunk"]
#[tokio::test]
async fn shell_monitor_stream_captures_output() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let task_dir = tempfile::tempdir().unwrap();
    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        let (_handle, stream) = ShellMonitorStream::new(
            "echo hello-from-shell",
            None,
            vec![],
            "test-shell",
            ShellMonitorConfig::default(),
        )
        .expect("failed to spawn shell monitor");
        let stream_arc = Arc::new(Mutex::new(stream));
        let task_store = Arc::new(JsonlTaskStore::new(task_dir.path().join("tasks")));
        let tool = WaitForExternalEventTool::new(
            stream_arc,
            serde_json::json!({"type": "shell_monitor", "label": "test-shell"}),
            Some(task_store),
            TaskId("task1".into()),
        );
        b.tool(Arc::new(tool) as Arc<dyn Tool>)
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        MULTI_TURN,
        harness.prompt(
            "Use the wait_for_external_event tool to wait for the output of a shell command. \
             After the tool returns, tell me what the shell output was.",
        ),
    )
    .await
    .expect("run should complete successfully");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "wait_for_external_event");
}
