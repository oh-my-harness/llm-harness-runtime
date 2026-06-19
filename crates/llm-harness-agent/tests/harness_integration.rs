//! Integration tests for AgentHarness covering the full framework flow:
//! prompt → session writes → tool call → fork/navigate → compaction override.

#![cfg(feature = "test-utils")]

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::{AgentHarness, AgentHarnessOptions, HarnessHooks};
use llm_harness_loop::{
    LlmClient,
    test_utils::{MockLlmClient, MockResponse, NoOpEnv},
};
use llm_harness_types::*;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_harness(responses: Vec<MockResponse>) -> AgentHarness {
    let client = Arc::new(MockLlmClient::new(responses));
    let env = Arc::new(NoOpEnv);
    let opts = AgentHarnessOptions::new("test-model");
    futures::executor::block_on(AgentHarness::new_in_memory(
        client as Arc<dyn LlmClient>,
        env,
        opts,
    ))
}

fn make_harness_with_tools(
    responses: Vec<MockResponse>,
    tools: Vec<Arc<dyn Tool>>,
) -> AgentHarness {
    let client = Arc::new(MockLlmClient::new(responses));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.tools = tools;
    futures::executor::block_on(AgentHarness::new_in_memory(
        client as Arc<dyn LlmClient>,
        env,
        opts,
    ))
}

fn text_content(msg: &AgentMessage) -> Option<String> {
    match msg {
        AgentMessage::User(u) => u.content.iter().find_map(|c| {
            if let ContentBlock::Text { text } = c {
                Some(format!("user:{}", text))
            } else {
                None
            }
        }),
        AgentMessage::Assistant(a) => a.content.iter().find_map(|c| {
            if let ContentBlock::Text { text } = c {
                Some(format!("assistant:{}", text))
            } else {
                None
            }
        }),
        _ => None,
    }
}

fn text_messages(ctx: &llm_harness_agent::BuiltContext) -> Vec<String> {
    ctx.messages.iter().filter_map(text_content).collect()
}

// ── Echo tool ─────────────────────────────────────────────────────────────────

/// Echoes its `input` parameter back as output.
struct EchoTool {
    schema: serde_json::Value,
}

impl EchoTool {
    fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": { "input": { "type": "string" } },
                "required": ["input"]
            }),
        }
    }
}

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes the input"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        let input = args
            .get("input")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Box::pin(async move {
            Ok(ToolResult {
                content: vec![ContentBlock::Text {
                    text: format!("echo:{}", input),
                }],
                details: serde_json::Value::Null,
                terminate: false,
            })
        })
    }
}

// ── Compaction override hook ──────────────────────────────────────────────────

/// Supplies a fixed `CompactionResult` so tests don't need a real LLM for compaction.
struct OverrideCompactHook {
    first_kept_entry: std::sync::Mutex<Option<EntryId>>,
}

impl OverrideCompactHook {
    fn new() -> Self {
        Self {
            first_kept_entry: std::sync::Mutex::new(None),
        }
    }

    fn set_first_kept(&self, id: EntryId) {
        *self.first_kept_entry.lock().unwrap() = Some(id);
    }
}

impl BeforeCompactHook for OverrideCompactHook {
    fn before_compact<'a>(
        &'a self,
        ctx: BeforeCompactCtx<'a>,
    ) -> BoxFuture<'a, BeforeCompactDecision> {
        let first_kept = self.first_kept_entry.lock().unwrap().unwrap();
        let summary = AgentMessage::CompactionSummary(CompactionSummaryMessage {
            summary: format!("summary of {} messages", ctx.messages.len()),
            timestamp: chrono::Utc::now(),
        });
        let result = CompactionResult {
            summary_message: summary,
            first_kept_entry: first_kept,
            tokens_before: 1000,
            tokens_after: 100,
        };
        Box::pin(async move { BeforeCompactDecision::Override(result) })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn prompt_writes_user_and_assistant_to_session() {
    let h = make_harness(vec![MockResponse::text("Hello!")]);
    h.prompt("greet me").await.unwrap();

    let ctx = h.build_context().await.unwrap();
    let texts = text_messages(&ctx);
    assert!(
        texts.iter().any(|t| t == "user:greet me"),
        "user message missing: {:?}",
        texts
    );
    assert!(
        texts.iter().any(|t| t == "assistant:Hello!"),
        "assistant message missing: {:?}",
        texts
    );
}

#[tokio::test]
async fn multiple_prompts_accumulate_in_session() {
    let h = make_harness(vec![
        MockResponse::text("reply one"),
        MockResponse::text("reply two"),
    ]);
    h.prompt("question one").await.unwrap();
    h.prompt("question two").await.unwrap();

    let ctx = h.build_context().await.unwrap();
    // user1, assistant1, user2, assistant2.
    assert!(
        ctx.messages.len() >= 4,
        "expected ≥4 messages, got {}",
        ctx.messages.len()
    );
}

#[tokio::test]
async fn round_trip_message_order_preserved() {
    let h = make_harness(vec![
        MockResponse::text("reply one"),
        MockResponse::text("reply two"),
    ]);
    h.prompt("ask one").await.unwrap();
    h.prompt("ask two").await.unwrap();

    let ctx = h.build_context().await.unwrap();
    let texts = text_messages(&ctx);
    assert_eq!(
        texts,
        vec![
            "user:ask one",
            "assistant:reply one",
            "user:ask two",
            "assistant:reply two",
        ],
        "unexpected order: {:?}",
        texts
    );
}

#[tokio::test]
async fn tool_call_writes_tool_result_to_session() {
    let responses = vec![
        // Turn 1: model calls echo tool.
        MockResponse::tool_use("call-1", "echo", r#"{"input":"ping"}"#),
        // Turn 2: model emits final text after seeing tool result.
        MockResponse::text("done"),
    ];
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool::new())];
    let h = make_harness_with_tools(responses, tools);

    h.prompt("use echo").await.unwrap();

    let ctx = h.build_context().await.unwrap();
    // Expect: user, assistant(tool_use), tool_result, assistant(text).
    assert!(
        ctx.messages.len() >= 3,
        "expected ≥3 messages, got {}",
        ctx.messages.len()
    );
    let has_tool_result = ctx.messages.iter().any(|m| {
        if let AgentMessage::ToolResult(tr) = m {
            tr.content.iter().any(|c| {
                if let ContentBlock::Text { text } = c {
                    text.contains("echo:ping")
                } else {
                    false
                }
            })
        } else {
            false
        }
    });
    assert!(has_tool_result, "echo tool result not found in session");
}

#[tokio::test]
async fn fork_and_navigate_produces_independent_branches() {
    let h = make_harness(vec![
        MockResponse::text("root reply"),
        MockResponse::text("branch A reply"),
        MockResponse::text("branch B reply"),
    ]);

    // Prompt 1: shared root.
    h.prompt("shared root").await.unwrap();

    let branches = h.list_branches().await.unwrap();
    assert_eq!(branches.len(), 1);
    let root_leaf = branches[0].leaf_id;

    // Fork at root_leaf and do branch A.
    h.fork_branch(root_leaf, Some("branch-A".to_string()))
        .await
        .unwrap();
    h.prompt("branch A question").await.unwrap();
    let ctx_a = h.build_context().await.unwrap();

    let has_a = ctx_a.messages.iter().any(|m| {
        if let AgentMessage::Assistant(a) = m {
            a.content
                .iter()
                .any(|c| matches!(c, ContentBlock::Text { text } if text.contains("branch A")))
        } else {
            false
        }
    });
    assert!(has_a, "branch A reply missing: {:?}", text_messages(&ctx_a));

    // Navigate back to root_leaf and do branch B.
    h.navigate_tree(root_leaf).await.unwrap();
    h.prompt("branch B question").await.unwrap();
    let ctx_b = h.build_context().await.unwrap();

    let has_b = ctx_b.messages.iter().any(|m| {
        if let AgentMessage::Assistant(a) = m {
            a.content
                .iter()
                .any(|c| matches!(c, ContentBlock::Text { text } if text.contains("branch B")))
        } else {
            false
        }
    });
    assert!(has_b, "branch B reply missing: {:?}", text_messages(&ctx_b));

    // Branch B context must NOT contain branch A reply.
    let b_has_a_reply = ctx_b.messages.iter().any(|m| {
        if let AgentMessage::Assistant(a) = m {
            a.content
                .iter()
                .any(|c| matches!(c, ContentBlock::Text { text } if text.contains("branch A")))
        } else {
            false
        }
    });
    assert!(
        !b_has_a_reply,
        "branch B context should not see branch A reply"
    );

    let all = h.list_branches().await.unwrap();
    assert!(all.len() >= 2, "expected ≥2 branches, got {}", all.len());
}

#[tokio::test]
async fn compaction_via_hook_override_inserts_summary() {
    let hook = Arc::new(OverrideCompactHook::new());

    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("reply")]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.hooks = HarnessHooks {
        before_compact: vec![hook.clone()],
        ..HarnessHooks::none()
    };
    let h = AgentHarness::new_in_memory(client, env, opts).await;

    h.prompt("question").await.unwrap();

    // Use the last written entry as first_kept_entry so compaction keeps nothing.
    let ctx_before = h.build_context().await.unwrap();
    assert!(!ctx_before.messages.is_empty());

    // Append a marker entry and tell the hook to keep from there.
    let kept_id = h
        .append_custom_entry("marker".into(), serde_json::json!({}))
        .await
        .unwrap();
    hook.set_first_kept(kept_id);

    let stats = h.compact().await.unwrap();
    assert_eq!(
        stats.tokens_before, 1000,
        "wrong tokens_before from override"
    );

    // After compaction, the first message must be the summary.
    let ctx_after = h.build_context().await.unwrap();
    assert!(
        matches!(
            ctx_after.messages.first(),
            Some(AgentMessage::CompactionSummary(_))
        ),
        "expected summary as first message; got {:?}",
        ctx_after.messages.first().map(std::mem::discriminant)
    );
}

#[tokio::test]
async fn model_change_reflected_in_session_context() {
    let h = make_harness(vec![]);
    h.set_model("claude-opus-4-7".into(), None).await.unwrap();

    let ctx = h.build_context().await.unwrap();
    assert_eq!(ctx.last_model.as_deref(), Some("claude-opus-4-7"));
}
