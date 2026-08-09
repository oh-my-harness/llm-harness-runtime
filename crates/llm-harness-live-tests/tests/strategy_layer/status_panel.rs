//! Tests: StatusPanelPlugin — `<agent_status>` XML injection and
//! the `todo_write` tool integration.
//!
//! Covers (spec §7):
//! - StatusPanelPlugin does not break a normal conversation
//! - `todo_write` tool is callable and surfaces in EventCapture::tool_calls()

use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, assert_tool_was_called, make_harness_with,
    skip_if_none, with_timeout,
};
use llm_harness_strategy::{StatusPanelConfig, StatusPanelPlugin};

// ── Tests ────────────────────────────────────────────────────────────────

/// StatusPanelPlugin installs hooks and tools but must not interfere with a
/// plain conversational turn. The conversation completes and produces
/// non-empty text.
#[tokio::test]
async fn status_bar_xml_appears() {
    let provider = LiveProvider::from_env();
    let entry = provider.openai.as_ref().or(provider.anthropic.as_ref());
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let (harness, _tmp) = make_harness_with(entry, |b| {
        b.install(&StatusPanelPlugin::new(StatusPanelConfig::default()))
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    with_timeout(SINGLE_TURN, harness.prompt("Say hello"))
        .await
        .expect("prompt should succeed");

    cap.settle().await;

    let text = cap.texts();
    assert!(!text.trim().is_empty(), "response text should be non-empty");
}

/// The `todo_write` tool registered by StatusPanelPlugin is callable by the
/// model. We prompt the model to create a todo list and assert the tool was
/// invoked via EventCapture::tool_calls().
#[tokio::test]
async fn todo_write_updates_status() {
    let provider = LiveProvider::from_env();
    let entry = provider.openai.as_ref().or(provider.anthropic.as_ref());
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let (harness, _tmp) = make_harness_with(entry, |b| {
        b.install(&StatusPanelPlugin::new(StatusPanelConfig::default()))
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        SINGLE_TURN,
        harness.prompt(
            "Use the todo_write tool to create a todo list with items: 'buy milk', 'walk dog'",
        ),
    )
    .await
    .expect("prompt should succeed");

    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "todo_write");
}
