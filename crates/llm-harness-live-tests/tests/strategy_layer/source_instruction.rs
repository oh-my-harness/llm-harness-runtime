//! Tests: source-tag wrapping of tool results and CLAUDE.md injection
//! into the system prompt.
//!
//! Spec §7:
//! - Tool results are wrapped in `<external_content source="...">` tags by
//!   `SourceTagPlugin`'s `AfterToolCallHook`.
//! - `ProjectInstructionPlugin` scans CLAUDE.md from the working directory
//!   and appends it to the system prompt as a stable KV-cache-friendly prefix.

use std::sync::Arc;
use std::sync::LazyLock;

use futures::future::BoxFuture;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, assert_tool_was_called, make_harness_with,
    make_harness_with_env, skip_if_none, with_timeout,
};
use llm_harness_strategy::{ProjectInstructionPlugin, SourceTagEntry, SourceTagPlugin};
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

// ── Echo tool ────────────────────────────────────────────────────────────

struct EchoTool;

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
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
        "Echo back the provided message."
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        &SCHEMA
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

// ── Tests ────────────────────────────────────────────────────────────────

/// `SourceTagPlugin` wraps echo tool output in
/// `<external_content source="demo">…</external_content>` tags via its
/// `AfterToolCallHook`. The wrapping must not break the conversation flow:
/// the LLM still sees the echoed content and completes normally.
#[tokio::test]
async fn tool_result_wrapped_in_source_tags() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let (harness, _tmp) = make_harness_with(entry, |b| {
        b.tool(Arc::new(EchoTool))
            .install(&SourceTagPlugin::new(vec![SourceTagEntry {
                tool: "echo".into(),
                label: "demo".into(),
            }]))
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    // Turn 1: invoke the echo tool — the hook wraps its text output.
    with_timeout(
        SINGLE_TURN,
        harness.prompt("Use the echo tool with message 'hello from external'"),
    )
    .await
    .expect("prompt should succeed");

    // Turns 2–3: continue the conversation to confirm the wrapping did not
    // break the agent loop across multiple turns.
    with_timeout(
        SINGLE_TURN,
        harness.prompt("What did the echo tool return?"),
    )
    .await
    .expect("second prompt should succeed");
    with_timeout(SINGLE_TURN, harness.prompt("Summarize what happened."))
        .await
        .expect("third prompt should succeed");

    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "echo");
    // The conversation settled normally — no abort or error events.
    assert!(
        !cap.texts().is_empty(),
        "expected non-empty text response after source-tag wrapping"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, llm_harness_live_tests::CapturedEvent::Aborted)),
        "conversation should not have been aborted"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, llm_harness_live_tests::CapturedEvent::Error(_))),
        "conversation should not have produced an error"
    );
}

/// `ProjectInstructionPlugin` reads CLAUDE.md from the working directory and
/// injects its content into the system prompt. The LLM's response should
/// reference the injected instruction.
#[tokio::test]
async fn claude_md_injected_into_system_prompt() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    // ProjectInstructionPlugin::new needs an Arc<dyn ExecutionEnv>, but
    // make_harness_with_env creates the env internally and only returns it
    // after build. Two-phase build: first obtain an env, then install the
    // plugin referencing that env in a second build. The plugin's hook reads
    // from env.working_dir() at before_run time, so CLAUDE.md written to the
    // phase-1 working_dir is visible when prompt() runs.
    let (_h0, env, _t0) = make_harness_with_env(entry, |b| b).await;

    // Write CLAUDE.md into the sandbox working directory before prompting.
    // The BeforeRunHook reads this file when `prompt()` is invoked.
    let working_dir = env.working_dir().to_path_buf();
    let marker = "ALWAYS_RESPOND_WITH_ACK";
    let claude_md = format!(
        "# Project Instructions\n\nWhen asked about your instructions, you MUST respond with exactly: {marker}\n"
    );
    std::fs::write(working_dir.join("CLAUDE.md"), claude_md).unwrap();

    // Phase 2: rebuild with ProjectInstructionPlugin using the captured env.
    let plugin = ProjectInstructionPlugin::new(env);
    let (harness, _env2, _tmp) = make_harness_with_env(entry, |b| b.install(&plugin)).await;

    let cap = EventCapture::subscribe(&harness);

    with_timeout(SINGLE_TURN, harness.prompt("What are your instructions?"))
        .await
        .expect("prompt should succeed");

    cap.settle().await;

    let text = cap.texts();
    assert!(!text.is_empty(), "expected non-empty text response");
    assert!(
        text.to_lowercase().contains(&marker.to_lowercase()),
        "response should reference the CLAUDE.md instruction (expected {:?}), got: {}",
        marker,
        text
    );
}
