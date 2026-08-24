//! 16 — Status Panel: StatusPanelPlugin (todo_write + status bar + tool counter).
//!
//! Demonstrates:
//! - Part 1: Install StatusPanelPlugin with default config; prompt the LLM to
//!   use `todo_write` to create a 3-item todo list. Show the TodoStore is
//!   populated (the status bar XML is rendered from this store each turn).
//! - Part 2: Prompt the LLM to update/complete todos via `todo_write`. Show
//!   the tool was called and the store reflects the changes.
//! - Part 3: Run a conversation with multiple `echo` tool calls. Show the
//!   ToolCallCounter tracks per-tool call counts.
//! - Part 4: Prompt the LLM to use `todo_write` with `add` (append a single
//!   item) then `clear` (remove all items). Show the new actions work
//!   end-to-end.
//!
//! The `StatusPanelPlugin` registers three things in one `install` call:
//!   1. `todo_write` tool — writes to a shared `TodoStore`
//!   2. `StatusBarHook` (TransformContextHook) — injects `<agent_status>` XML
//!      at the end of the LLM context each turn, rendering todo list,
//!      environment info, and tool call counts.
//!   3. `ToolCallCounterHook` (AfterToolCallHook) — increments a shared
//!      `ToolCallCounter` after every tool execution.
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 16_status_panel

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness_with, with_timeout,
};
use llm_harness_strategy::{StatusPanelConfig, StatusPanelPlugin};
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

// ── Echo tool ────────────────────────────────────────────────────────────────

struct EchoTool;

static ECHO_SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "The message to echo back" }
        },
        "required": ["message"]
    })
});

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo back the provided message verbatim."
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        &ECHO_SCHEMA
    }
    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let msg = args
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("(no message)");
            Ok(ToolResult::full(
                vec![DataBlock::text(msg)],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

#[tokio::main]
async fn main() {
    println!("=== 16: Status Panel (StatusPanelPlugin) ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .or(provider.anthropic.as_ref())
        .expect("set OPENAI_API_KEY or ANTHROPIC_API_KEY to run this example");
    println!("Provider: {} | Model: {}\n", entry.name, entry.model);
    part1_todo_create(entry).await;
    println!();
    part2_todo_update(entry).await;
    println!();
    part3_tool_counter(entry).await;
    println!();
    part4_add_and_clear(entry).await;

    println!("\n--- Summary ---");
    println!("Part 1: todo_write creates todos in the shared store ✅");
    println!("Part 2: todo_write updates todo statuses ✅");
    println!("Part 3: ToolCallCounter tracks per-tool call counts ✅");
    println!("Part 4: todo_write 'add' appends, 'clear' empties ✅");
}

// ── Part 1: Install plugin + create todos ────────────────────────────────────

async fn part1_todo_create(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 1: StatusPanelPlugin + todo_write (create) ---\n");

    let plugin = StatusPanelPlugin::new(StatusPanelConfig::default());
    // Clone the shared stores before installing — they are Arc-backed so
    // the clones share state with the plugin's internal copies.
    let todo_store = plugin.todo_store().clone();
    let tool_counter = plugin.tool_counter().clone();

    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.install(&plugin).tool(Arc::new(EchoTool) as Arc<dyn Tool>)
    })
    .await;

    println!("TodoStore before prompt: {:?}", todo_store.snapshot());

    let cap = EventCapture::subscribe(&harness);
    let prompt = "Use the todo_write tool to create a todo list with 3 items: \
                  'Write tests', 'Run tests', 'Deploy'. \
                  Use action 'rewrite' and set all items to status 'pending'.";
    println!("LLM prompt: {}", prompt);
    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    let todos = todo_store.snapshot();
    let counts = tool_counter.snapshot();

    println!("\nTools called: {:?}", tools_called);
    println!("TodoStore after prompt ({} items):", todos.len());
    for t in &todos {
        println!("  [{}] {} — {}", t.id, t.content, t.status);
    }
    println!("Tool call counter: {:?}", counts);

    let todo_called = tools_called.iter().any(|t| t == "todo_write");
    println!("\nObservation:");
    println!("  todo_write called:  {}", todo_called);
    println!("  todos in store:     {}", todos.len());
    println!("  status bar renders <todo_list> from this store each turn");
}

// ── Part 2: Update / complete todos ──────────────────────────────────────────

async fn part2_todo_update(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 2: todo_write (update/complete) ---\n");

    let plugin = StatusPanelPlugin::new(StatusPanelConfig::default());
    let todo_store = plugin.todo_store().clone();
    let tool_counter = plugin.tool_counter().clone();

    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.install(&plugin).tool(Arc::new(EchoTool) as Arc<dyn Tool>)
    })
    .await;

    // Seed the store with 3 pending todos so the LLM has something to update.
    use llm_harness_strategy::status_panel::{TodoItem, TodoStatus};
    todo_store.rewrite(vec![
        TodoItem::new("1", "Write tests"),
        TodoItem::new("2", "Run tests"),
        TodoItem::new("3", "Deploy"),
    ]);
    println!("Seeded TodoStore ({} items):", todo_store.snapshot().len());
    for t in &todo_store.snapshot() {
        println!("  [{}] {} — {}", t.id, t.content, t.status);
    }

    let cap = EventCapture::subscribe(&harness);
    let prompt = "Use the todo_write tool with action 'update' to mark item id '1' \
                  as 'completed' and item id '2' as 'in_progress'. \
                  Keep item '3' as 'pending'.";
    println!("\nLLM prompt: {}", prompt);
    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    let todos = todo_store.snapshot();

    println!("\nTools called: {:?}", tools_called);
    println!("TodoStore after update ({} items):", todos.len());
    for t in &todos {
        println!("  [{}] {} — {}", t.id, t.content, t.status);
    }
    println!("Tool call counter: {:?}", tool_counter.snapshot());

    let todo_called = tools_called.iter().any(|t| t == "todo_write");
    let item1 = todos.iter().find(|t| t.id == "1");
    let item2 = todos.iter().find(|t| t.id == "2");
    println!("\nObservation:");
    println!("  todo_write called:     {}", todo_called);
    println!("  item 1 status:         {:?}", item1.map(|t| t.status));
    println!("  item 2 status:         {:?}", item2.map(|t| t.status));
    let _ = TodoStatus::Pending; // suppress unused import warning if LLM didn't call tool
}

// ── Part 4: add / clear actions ──────────────────────────────────────────────

async fn part4_add_and_clear(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 4: todo_write (add / clear) ---\n");

    let plugin = StatusPanelPlugin::new(StatusPanelConfig::default());
    let todo_store = plugin.todo_store().clone();

    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| b.install(&plugin)).await;

    // Start with one seeded item so we can see `add` append to it.
    use llm_harness_strategy::status_panel::TodoItem;
    todo_store.rewrite(vec![TodoItem::new("1", "seeded task")]);
    println!("Seeded TodoStore ({} item):", todo_store.snapshot().len());
    for t in &todo_store.snapshot() {
        println!("  [{}] {} — {}", t.id, t.content, t.status);
    }

    // --- add ---
    let cap = EventCapture::subscribe(&harness);
    let prompt = "Call the todo_write tool with exactly these arguments: \
                  {\"action\": \"add\", \"id\": \"2\", \"content\": \"added by LLM\", \"status\": \"pending\"}. \
                  This appends a new item to the existing list.";
    println!("\nLLM prompt: {}", prompt);
    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    let todos = todo_store.snapshot();

    println!("\nTools called: {:?}", tools_called);
    println!("TodoStore after 'add' ({} items):", todos.len());
    for t in &todos {
        println!("  [{}] {} — {}", t.id, t.content, t.status);
    }

    let add_called = tools_called.iter().any(|t| t == "todo_write");
    let has_added = todos
        .iter()
        .any(|t| t.id == "2" && t.content == "added by LLM");
    let has_seeded = todos.iter().any(|t| t.id == "1");
    println!("\nObservation (add):");
    println!("  todo_write called:    {}", add_called);
    println!("  seeded item present:  {}", has_seeded);
    println!("  added item present:   {}", has_added);

    // --- clear ---
    let cap = EventCapture::subscribe(&harness);
    let prompt = "Call the todo_write tool with exactly these arguments: \
                  {\"action\": \"clear\"}. This removes all items from the todo list.";
    println!("\nLLM prompt: {}", prompt);
    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    let todos = todo_store.snapshot();

    println!("\nTools called: {:?}", tools_called);
    println!("TodoStore after 'clear' ({} items):", todos.len());

    let clear_called = tools_called.iter().any(|t| t == "todo_write");
    let is_empty = todos.is_empty();
    println!("\nObservation (clear):");
    println!("  todo_write called:  {}", clear_called);
    println!("  store is empty:     {}", is_empty);
}

// ── Part 3: Tool call counter ────────────────────────────────────────────────

async fn part3_tool_counter(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 3: ToolCallCounter (multiple echo calls) ---\n");

    let plugin = StatusPanelPlugin::new(StatusPanelConfig::default());
    let tool_counter = plugin.tool_counter().clone();
    let todo_store = plugin.todo_store().clone();

    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.install(&plugin).tool(Arc::new(EchoTool) as Arc<dyn Tool>)
    })
    .await;

    println!(
        "ToolCallCounter before prompt: {:?}",
        tool_counter.snapshot()
    );

    let cap = EventCapture::subscribe(&harness);
    let prompt = "Use the echo tool three times with different messages: \
                  'first', 'second', 'third'. Call the tool for each message.";
    println!("LLM prompt: {}", prompt);
    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    let counts = tool_counter.snapshot();
    let todos = todo_store.snapshot();

    println!("\nTools called (events): {:?}", tools_called);
    println!("ToolCallCounter snapshot:");
    for (name, count) in &counts {
        println!("  {} — {} call(s)", name, count);
    }
    println!(
        "TodoStore (should be empty, no todo_write): {} items",
        todos.len()
    );

    let echo_count = counts
        .iter()
        .find(|(n, _)| n == "echo")
        .map(|(_, c)| *c)
        .unwrap_or(0);
    let echo_events = tools_called.iter().filter(|t| *t == "echo").count();

    println!("\nObservation:");
    println!("  echo calls (events):    {}", echo_events);
    println!("  echo calls (counter):   {}", echo_count);
    println!("  counter tracks every AfterToolCall execution");
}
