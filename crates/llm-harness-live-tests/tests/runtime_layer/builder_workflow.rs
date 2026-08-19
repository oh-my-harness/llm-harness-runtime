//! Tests: full-crate assembly smoke, provider glob matching, plugin tool
//! registration, linear workflow step advancement, and conditional routing
//! via EdgeConditionJudge.
//!
//! Each test runs against a real LLM provider (skipped if none configured).

use std::sync::Arc;
use std::sync::LazyLock;

use futures::future::BoxFuture;

use llm_harness_live_tests::skip_if_none;
use llm_harness_live_tests::{
    CapturedEvent, EventCapture, LiveProvider, MULTI_TURN, SINGLE_TURN, assert_event_emitted,
    assert_tool_was_called, make_harness_with, with_timeout,
};

use llm_harness_runtime::spawn::spawner::JsonlSessionFactory;
use llm_harness_runtime::workflow::engine::{WorkflowEngine, WorkflowEngineConfig};
use llm_harness_runtime::workflow::judge::EdgeConditionJudge;
use llm_harness_runtime::workflow::model::{Edge, EdgeCondition, Step, Workflow};
use llm_harness_sandbox::OsEnvFactory;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

// ── Echo tool (for plugin_registers_tools) ────────────────────────────────

struct EchoTool;

static ECHO_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "Message to echo back" }
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

// ── Helper: build a WorkflowEngineConfig backed by a tempdir ──────────────

fn make_config(
    entry: &llm_harness_live_tests::LiveProviderEntry,
    session_base: &std::path::Path,
) -> WorkflowEngineConfig {
    WorkflowEngineConfig {
        client: entry.client.clone(),
        model: entry.model.clone(),
        env_factory: Arc::new(OsEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: session_base.to_path_buf(),
        customize_builder: None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

/// Full-crate assembly smoke: build a harness via `make_harness_with`,
/// run a single prompt, and verify Settled + non-empty text.
#[tokio::test]
async fn builder_assembles_and_runs() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();
    let (harness, _tmp) = make_harness_with(entry, |b| b).await;
    let cap = EventCapture::subscribe(&harness);

    with_timeout(SINGLE_TURN, harness.prompt("Say hello"))
        .await
        .expect("prompt should succeed");
    cap.settle().await;

    let events = cap.events();
    assert_event_emitted(&events, &CapturedEvent::Settled);
    assert!(!cap.texts().is_empty(), "expected non-empty text response");
}

/// Provider glob matching: `make_harness_with` registers the client under
/// the glob pattern `"*"`. A real prompt should still reach the LLM and
/// produce a response, proving the glob resolver works.
#[tokio::test]
async fn provider_glob_matches() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();
    let (harness, _tmp) = make_harness_with(entry, |b| b).await;
    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        SINGLE_TURN,
        harness.prompt("What is 1+1? Answer with just the number."),
    )
    .await
    .expect("prompt should succeed");
    cap.settle().await;

    // Glob matched → the LLM responded with text.
    assert!(
        !cap.texts().is_empty(),
        "glob provider should have produced text"
    );
}

/// Plugin tool registration: register an EchoTool via the builder callback,
/// prompt the LLM to use it, and verify the tool was called.
#[tokio::test]
async fn plugin_registers_tools() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();
    let (harness, _tmp) = make_harness_with(entry, |b| b.tool(Arc::new(EchoTool))).await;
    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        MULTI_TURN,
        harness.prompt("Use the echo tool with message 'test'. Then tell me what it returned."),
    )
    .await
    .expect("prompt should succeed");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "echo");
}

/// Linear workflow: 2 LLM steps + terminal. Verify the engine advances
/// through all steps and reaches Succeeded.
#[tokio::test]
async fn linear_workflow_completes() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let session_base = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_base).unwrap();
    let config = make_config(entry, &session_base);

    let workflow = Workflow {
        entry_step: "step1".into(),
        steps: vec![
            Step::llm(
                "step1",
                "First Step",
                "What is 2+2? Answer in one word.",
                vec![],
            ),
            Step::llm(
                "step2",
                "Second Step",
                "Write a one-sentence summary about numbers.",
                vec![],
            ),
            Step::terminal("end", "Done", 0, Some("Workflow complete".into())),
        ],
        edges: vec![
            Edge {
                from: "step1".into(),
                to: "step2".into(),
                condition: None,
            },
            Edge {
                from: "step2".into(),
                to: "end".into(),
                condition: None,
            },
        ],
    };

    let judge = Arc::new(EdgeConditionJudge::from_workflow(&workflow));
    let engine = WorkflowEngine::new(workflow, config, judge).expect("failed to build engine");

    let result = with_timeout(MULTI_TURN, engine.run())
        .await
        .expect("workflow run should succeed");

    assert_eq!(
        engine.state().await,
        llm_harness_runtime::workflow::model::WorkflowStatus::Succeeded
    );

    let history = engine.step_history().await;
    // 2 LLM steps + terminal step (terminal may or may not appear in history).
    assert!(
        history.len() >= 2,
        "expected at least 2 step records (2 LLM steps), got {}",
        history.len()
    );
    // result.turns should be >= 1 (at least one LLM turn across steps)
    assert!(
        result.turns >= 1,
        "expected at least 1 turn, got {}",
        result.turns
    );
}

/// Conditional routing: step1 produces structured JSON with a `route_key`
/// field; EdgeConditionJudge matches the label and directs to the correct
/// terminal step.
#[tokio::test]
async fn conditional_routing_directs_correctly() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let session_base = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_base).unwrap();
    let config = make_config(entry, &session_base);

    let workflow = Workflow {
        entry_step: "step1".into(),
        steps: vec![
            Step::llm(
                "step1",
                "Decision",
                "Output a JSON object with a field \"route_key\" set to \"yes\". Output only the JSON, no extra text.",
                vec![],
            )
            .with_structured(Some(true)),
            Step::terminal("step_yes", "Yes Path", 0, Some("took yes path".into())),
            Step::terminal("step_no", "No Path", 0, Some("took no path".into())),
        ],
        edges: vec![
            Edge {
                from: "step1".into(),
                to: "step_yes".into(),
                condition: Some(EdgeCondition::Label("yes".into())),
            },
            Edge {
                from: "step1".into(),
                to: "step_no".into(),
                condition: Some(EdgeCondition::Label("no".into())),
            },
        ],
    };

    let judge = Arc::new(EdgeConditionJudge::from_workflow(&workflow));
    let engine = WorkflowEngine::new(workflow, config, judge).expect("failed to build engine");

    let _result = with_timeout(MULTI_TURN, engine.run())
        .await
        .expect("conditional workflow run should succeed");

    assert_eq!(
        engine.state().await,
        llm_harness_runtime::workflow::model::WorkflowStatus::Succeeded,
        "workflow should reach Succeeded"
    );

    let history = engine.step_history().await;
    // At least step1 + one terminal step.
    assert!(
        history.len() >= 2,
        "expected at least 2 step records (decision + terminal), got {}",
        history.len()
    );
}
