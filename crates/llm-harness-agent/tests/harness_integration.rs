//! Integration tests for AgentHarness covering the full framework flow:
//! prompt → session writes → tool call → fork/navigate → compaction override.

#![cfg(feature = "test-utils")]

use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_agent::{AgentHarness, AgentHarnessOptions};
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
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        let input = args
            .get("input")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Box::pin(async move {
            Ok(ToolResult::full(
                vec![DataBlock::text(format!("echo:{}", input))],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

struct EphemeralReadTool;

impl Tool for EphemeralReadTool {
    fn name(&self) -> &str {
        "ephemeral_read"
    }

    fn description(&self) -> &str {
        "returns a body that must not be persisted"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::LazyLock<serde_json::Value> =
            std::sync::LazyLock::new(|| serde_json::json!({"type": "object"}));
        &SCHEMA
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async {
            Ok(ToolResult::ephemeral(
                vec![DataBlock::text("private document body")],
                "Read document ref doc-1",
                serde_json::json!({"ref": "doc-1"}),
                false,
            ))
        })
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
                if let DataBlock::Text { text, .. } = c {
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
async fn ephemeral_result_is_full_for_current_run_and_projected_in_session() {
    use llm_adapter::types::{Message, RequestContent};

    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use("call-1", "ephemeral_read", "{}"),
        MockResponse::text("answer based on document"),
    ]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.tools = vec![Arc::new(EphemeralReadTool)];
    let harness =
        AgentHarness::new_in_memory(client.clone() as Arc<dyn LlmClient>, env, opts).await;

    harness.prompt("read it").await.unwrap();

    {
        let requests = client.captured_requests.lock();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].messages().iter().any(|message| {
            matches!(
                message,
                Message::Tool { content, .. }
                    if content.iter().any(|block| matches!(
                        block,
                        RequestContent::Text(text) if text == "private document body"
                    ))
            )
        }));
    }

    let context = harness.build_context().await.unwrap();
    let persisted = context
        .messages
        .iter()
        .find_map(|message| match message {
            AgentMessage::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("tool result must remain paired in Session");
    assert_eq!(persisted.projection.kind, ToolProjectionKind::Ephemeral);
    assert!(matches!(
        persisted.content.as_slice(),
        [DataBlock::Text { text, .. }] if text == "Read document ref doc-1"
    ));
    assert!(
        !serde_json::to_string(persisted)
            .unwrap()
            .contains("private document body")
    );
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
async fn model_change_reflected_in_session_context() {
    let h = make_harness(vec![]);
    h.set_model("claude-opus-4-7".into(), None).await.unwrap();

    let ctx = h.build_context().await.unwrap();
    assert_eq!(ctx.last_model.as_deref(), Some("claude-opus-4-7"));
}

/// A tool that opts into duration reporting. Regression for issue #139:
/// with NO hooks registered, loop_driver must still wrap tools with
/// HookedTool because any tool opts in — otherwise the annotation silently
/// never reaches the LLM.
struct TimedEchoTool;

impl Tool for TimedEchoTool {
    fn name(&self) -> &str {
        "timed_echo"
    }
    fn description(&self) -> &str {
        "Echoes the input, reports duration"
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        S.get_or_init(|| serde_json::json!({}))
    }
    fn report_duration(&self) -> bool {
        true
    }
    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async {
            Ok(ToolResult::full(
                vec![DataBlock::text("timed echo")],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

/// A tool that opts into duration reporting and panics on the first N calls.
/// Regression test for the panic-retry path: after a tool panic mid-run, the
/// run_loop cleanup must still restore `state.tools`, otherwise the agent-team
/// catch_unwind retry re-wraps the already-wrapped tools and double-fires
/// hooks / double-appends duration annotations.
struct PanicOnceTool {
    calls: std::sync::atomic::AtomicUsize,
}

impl Tool for PanicOnceTool {
    fn name(&self) -> &str {
        "panic_once"
    }
    fn description(&self) -> &str {
        "Panics on first call, succeeds on second"
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        S.get_or_init(|| serde_json::json!({}))
    }
    fn report_duration(&self) -> bool {
        true
    }
    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                panic!("tool blew up");
            }
            Ok(ToolResult::full(
                vec![DataBlock::text("post-panic echo")],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

#[tokio::test]
async fn duration_annotation_wraps_tools_without_hooks() {
    let responses = vec![
        MockResponse::tool_use("call-1", "timed_echo", "{}"),
        MockResponse::text("done"),
    ];
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(TimedEchoTool)];
    let h = make_harness_with_tools(responses, tools);

    h.prompt("use timed echo").await.unwrap();

    // The model-visible tool result (captured request) must carry the
    // annotation — proves the wrap happened with zero hooks configured.
    let ctx = h.build_context().await.unwrap();
    let annotated = ctx.messages.iter().any(|m| {
        if let AgentMessage::ToolResult(tr) = m {
            tr.content
                .iter()
                .any(|c| matches!(c, DataBlock::Text { text, .. } if text.contains("[duration: ")))
        } else {
            false
        }
    });
    assert!(
        annotated,
        "duration annotation missing from persisted tool result: {ctx:?}"
    );
}

/// Regression for the panic-retry path: a tool panic mid-run must NOT leave
/// HookedTool wrappers in `state.tools`, otherwise the next run (agent-team's
/// catch_unwind retry re-runs the harness) re-wraps the already-wrapped
/// tools — double-firing hooks and double-appending duration annotations.
#[tokio::test]
async fn panic_mid_run_does_not_double_wrap_tools() {
    // Run 1: tool_use → panic. Run 2: same tool_use → succeeds, then text.
    let responses = vec![
        MockResponse::tool_use("call-1", "panic_once", "{}"),
        MockResponse::tool_use("call-2", "panic_once", "{}"),
        MockResponse::text("done"),
    ];
    let tool = Arc::new(PanicOnceTool {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let h = make_harness_with_tools(responses, vec![tool.clone()]);

    // Run 1 panics inside the tool (propagates through HookedTool →
    // execute_tool_batch's join_all → drive_loop → run_loop's catch_unwind,
    // which resume_unwinds). agent-team wraps harness.run in its own
    // catch_unwind — mirror that here.
    let run1 = std::panic::AssertUnwindSafe(h.prompt("trigger the panic tool"))
        .catch_unwind()
        .await;
    assert!(run1.is_err(), "expected run to panic");

    // Run 2 (agent-team's post-panic retry re-runs the harness the same way).
    h.prompt("retry after panic").await.unwrap();

    // The second run's tool result must carry EXACTLY ONE duration
    // annotation: double wrapping would append two.
    let ctx = h.build_context().await.unwrap();
    let mut annotations = 0;
    for m in &ctx.messages {
        if let AgentMessage::ToolResult(tr) = m {
            annotations += tr
                .content
                .iter()
                .filter(|c| matches!(c, DataBlock::Text { text, .. } if text.starts_with("[duration: ")))
            .count();
        }
    }
    assert_eq!(
        annotations, 1,
        "expected exactly one duration annotation, got {annotations}: {ctx:?}"
    );
}

// ── Steer / teardown boundary (lost-steer race regression) ────────────────────

/// Gate mounted as the run's `ShouldStopHook`: the loop parks inside the hook
/// at its FINAL stop decision — after `agent_loop` has already drained the
/// steer channel one last time at that boundary — until the test releases it.
///
/// The gate's `steer_tx` handle is the harness sender captured from the
/// BeforeRunCtx's resources; we instead grab the live sender by calling
/// `steer_message` on the harness (which forwards to the run's live channel).
struct TeardownGate {
    /// Set once the loop is parked at the final stop decision.
    parked: Arc<std::sync::atomic::AtomicBool>,
    /// Test releases the gate to let the run finish.
    release: Arc<tokio::sync::Notify>,
    /// Gate parks only the FIRST stop decision; later runs stop freely.
    armed: std::sync::atomic::AtomicBool,
}

impl llm_harness_types::ShouldStopHook for TeardownGate {
    fn should_stop<'a>(
        &'a self,
        _ctx: llm_harness_types::ShouldStopCtx<'a>,
    ) -> BoxFuture<'a, bool> {
        let parked = Arc::clone(&self.parked);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            // Park only the first stop decision (the instrumented run);
            // later runs (consuming the salvage) must proceed normally.
            if self
                .armed
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                parked.store(true, std::sync::atomic::Ordering::SeqCst);
                release.notified().await;
            }
            true
        })
    }
}

fn gate_user_text(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        timestamp: chrono::Utc::now(),
    })
}

/// Race reproduction without wall-clock sleeps driving the ordering:
///
/// 1. run's single turn completes; `agent_loop` yields TurnEnd.
/// 2. should_stop hook parks the loop AFTER the loop's final steer drain.
/// 3. test sends a steer via `steer_message` (returns Ok — sender alive).
/// 4. test releases the gate; the run exits; the receiver is dropped.
///
/// Before the fix the steer vanished. Now run_loop must salvage it into
/// queued_next_turn, and the next prompt must consume it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_after_final_drain_is_salvaged_into_next_run() {
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("first done")]));
    let mut opts = AgentHarnessOptions::new("test-model");
    let parked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    opts.hooks.should_stop.push(Arc::new(TeardownGate {
        parked: Arc::clone(&parked),
        release: Arc::clone(&release),
        armed: std::sync::atomic::AtomicBool::new(true),
    }));
    let h = Arc::new(
        AgentHarness::new_in_memory(
            Arc::clone(&client) as Arc<dyn LlmClient>,
            Arc::new(NoOpEnv),
            opts,
        )
        .await,
    );

    let run1 = {
        let h = Arc::clone(&h);
        tokio::spawn(async move { h.prompt("run one").await })
    };

    // Deterministic parking point: the mock has been called once AND the
    // hook reports it is inside the final stop decision.
    for _ in 0..1000 {
        if parked.load(std::sync::atomic::Ordering::SeqCst)
            && client.call_count.load(std::sync::atomic::Ordering::SeqCst) == 1
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(
        parked.load(std::sync::atomic::Ordering::SeqCst),
        "loop never reached its final stop decision"
    );

    // The loop is parked at the final stop decision — past the turn-boundary
    // drain, before the final drain. A steer now gets Ok from try_send —
    // exactly the racing monitor send.
    h.steer_message(gate_user_text("late steer at teardown boundary"))
        .expect("live run must accept the steer");

    // Exit the run WITHOUT letting the loop's final drain consume the steer
    // as a follow-up turn: abort the harness, then release the gate. The
    // generator's post-run salvage must recover the steer instead of
    // dropping it. (Release after abort: the cancelled token is what forces
    // the salvage path instead of a live follow-up turn.)
    h.abort();
    release.notify_one();
    run1.await.unwrap().expect("aborted run must return Ok");

    // Invariant A-or-B: the steer was consumed by the run OR is recoverable.
    // The abort path guarantees the salvage branch; assert it explicitly.
    assert!(
        h.has_queued_messages(),
        "steer sent before the receiver dropped must be salvaged, not lost"
    );

    // Next run consumes the salvaged message first. The harness's mock
    // client falls back to a plain text response once presets are exhausted.
    h.prompt("run two").await.expect("run two must succeed");

    let ctx = h.build_context().await.unwrap();
    let steered = ctx.messages.iter().any(|m| {
        matches!(m, AgentMessage::User(u) if u.content.iter().any(
            |c| matches!(c, ContentBlock::Text { text } if text.contains("late steer"))
        ))
    });
    assert!(
        steered,
        "salvaged steer must appear in the next run's context: {:?}",
        ctx.messages
    );
    assert!(
        !h.has_queued_messages(),
        "salvaged steer must be consumed exactly once by the next run"
    );
}
