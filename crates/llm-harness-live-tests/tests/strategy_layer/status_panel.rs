//! Tests: StatusPanelPlugin — `<agent_status>` XML injection and
//! the `todo_write` tool integration.
//!
//! Covers (spec §7):
//! - StatusPanelPlugin does not break a normal conversation
//! - `todo_write` tool is callable and surfaces in EventCapture::tool_calls()

use llm_harness_live_tests::{
    EventCapture, LiveProvider, MULTI_TURN, SINGLE_TURN, assert_tool_was_called, make_harness_with,
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

/// The `add` action appends a single item to the todo list.
///
/// We prompt the LLM to call `todo_write` with `action: "add"`, then assert
/// the tool was called and the store now contains the added item. This
/// verifies the new `add` path works end-to-end in a live conversation
/// (LLM discovers the action from the tool schema, the tool processes it,
/// and the store is updated).
#[tokio::test]
async fn todo_write_add_appends_item() {
    let provider = LiveProvider::from_env();
    let entry = provider.openai.as_ref().or(provider.anthropic.as_ref());
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let plugin = StatusPanelPlugin::new(StatusPanelConfig::default());
    let todo_store = plugin.todo_store().clone();

    let (harness, _tmp) = make_harness_with(entry, |b| b.install(&plugin)).await;

    // Start empty — the LLM must create an item via `add`.
    assert!(todo_store.snapshot().is_empty());

    let cap = EventCapture::subscribe(&harness);
    with_timeout(
        MULTI_TURN,
        harness.prompt(
            "Call the todo_write tool with exactly these arguments: \
             {\"action\": \"add\", \"id\": \"1\", \"content\": \"added by LLM\", \"status\": \"pending\"}. \
             This appends a new item to the todo list.",
        ),
    )
    .await
    .expect("prompt should succeed");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "todo_write");

    let todos = todo_store.snapshot();
    assert!(
        todos
            .iter()
            .any(|t| t.id == "1" && t.content == "added by LLM"),
        "added item should be in store: {:?}",
        todos
    );
}

/// The `clear` action removes all items from the store.
#[tokio::test]
async fn todo_write_clear_empties_list() {
    let provider = LiveProvider::from_env();
    let entry = provider.openai.as_ref().or(provider.anthropic.as_ref());
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let plugin = StatusPanelPlugin::new(StatusPanelConfig::default());
    let todo_store = plugin.todo_store().clone();

    let (harness, _tmp) = make_harness_with(entry, |b| b.install(&plugin)).await;

    // Seed items so we can verify `clear` actually empties the list.
    use llm_harness_strategy::status_panel::TodoItem;
    todo_store.rewrite(vec![
        TodoItem::new("1", "task A"),
        TodoItem::new("2", "task B"),
    ]);
    assert_eq!(todo_store.snapshot().len(), 2);

    let cap = EventCapture::subscribe(&harness);
    with_timeout(
        MULTI_TURN,
        harness.prompt(
            "Call the todo_write tool with exactly these arguments: \
             {\"action\": \"clear\"}. This removes all items from the todo list.",
        ),
    )
    .await
    .expect("prompt should succeed");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "todo_write");

    let todos = todo_store.snapshot();
    assert!(
        todos.is_empty(),
        "store should be empty after clear: {:?}",
        todos
    );
}
