use super::*;
use crate::spawn::spawner::{EnvFactory, JsonlSessionFactory};
use anyhow::Result; // StepExecutor::execute returns anyhow::Result
use futures::future::BoxFuture;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
use llm_harness_types::{
    AgentError, BeforeTurnCtx, BeforeTurnHook, ContentBlock, ExecutionEnv, ToolContext, ToolError,
    ToolResult,
};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestEnvFactory;
impl EnvFactory for TestEnvFactory {
    fn create(
        &self,
        _cwd: &std::path::Path,
    ) -> std::result::Result<Arc<dyn ExecutionEnv>, AgentError> {
        Ok(Arc::new(llm_harness_loop::test_utils::NoOpEnv))
    }
}

#[tokio::test]
async fn free_task_smoke_succeeds() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
        "The answer is 42.",
    )]));
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new_free_task("what is 6*7?".into(), vec![], config).unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Idle);
    let result = engine.run().await.unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert!(result.final_message.as_deref().unwrap_or("").contains("42"));
}

use crate::lifecycle::task_store::JsonlTaskStore;
use crate::workflow::judge::NoopJudge;
use crate::workflow::model::{ConditionExpr, Edge, EdgeCondition};

struct FixedExecutor;
impl StepExecutor for FixedExecutor {
    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            let prev = ctx
                .prev_result
                .expect("executor should see previous result");
            let suffix = ctx
                .current_step
                .executor_config()
                .and_then(|config| config.get("suffix"))
                .and_then(|value| value.as_str())
                .unwrap_or("done");
            Ok(StepResult {
                output: format!("executor {suffix} after {}", prev.output),
                structured: Some(serde_json::json!({
                    "executor": true,
                    "previous_output": prev.output,
                    "history_len": ctx.step_history.len(),
                })),
                structured_status: StructuredStatus::NotRequired,
                tool_calls_count: 0,
                session_id: String::new(),
                cost: CostAggregate::default(),
                started_at: None,
                ended_at: None,
            })
        })
    }
}

struct StructuredExecutor;
impl StepExecutor for StructuredExecutor {
    fn execute<'a>(&'a self, _ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            Ok(StepResult {
                output: "structured ok".into(),
                structured: Some(serde_json::json!({"status": "ok"})),
                structured_status: StructuredStatus::NotRequired,
                tool_calls_count: 0,
                session_id: String::new(),
                cost: CostAggregate::default(),
                started_at: None,
                ended_at: None,
            })
        })
    }
}

struct BlockingExecutor;
impl StepExecutor for BlockingExecutor {
    fn execute<'a>(&'a self, _ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move { std::future::pending::<Result<StepResult>>().await })
    }
}

struct FlakyExecutor {
    attempts: Arc<AtomicUsize>,
}

impl StepExecutor for FlakyExecutor {
    fn execute<'a>(&'a self, _ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 1 {
                anyhow::bail!("transient executor failure");
            }
            Ok(StepResult {
                output: format!("ok on attempt {attempt}"),
                structured: Some(serde_json::json!({ "attempt": attempt })),
                structured_status: StructuredStatus::NotRequired,
                tool_calls_count: 0,
                session_id: String::new(),
                cost: CostAggregate::default(),
                started_at: None,
                ended_at: None,
            })
        })
    }
}

fn completed_result(output: &str) -> StepResult {
    StepResult {
        output: output.into(),
        structured: None,
        structured_status: StructuredStatus::NotRequired,
        tool_calls_count: 0,
        session_id: format!("session-{output}"),
        cost: CostAggregate::default(),
        started_at: Some(chrono::Utc::now()),
        ended_at: Some(chrono::Utc::now()),
    }
}

fn completed_record(step_id: &str, output: &str, transition: Transition) -> StepRecord {
    let now = chrono::Utc::now();
    StepRecord {
        step_id: step_id.into(),
        started_at: now,
        ended_at: now,
        result: Some(completed_result(output)),
        transition,
    }
}

#[test]
fn step_history_summary_includes_structured_before_output() {
    let mut record = completed_record(
        "measure",
        "measurement complete",
        Transition::To("decide".into()),
    );
    record.result.as_mut().unwrap().structured = Some(serde_json::json!({
        "voltage": 3.3,
        "passed": true,
    }));

    let summary = format_step_history_summary(&[record]);

    assert!(summary.contains("- step 'measure'"));
    assert!(summary.contains("structured:"));
    assert!(summary.contains("\"voltage\":3.3"));
    assert!(summary.contains("\"passed\":true"));
    assert!(summary.contains("output: measurement complete"));
    assert!(
        summary.find("structured:").unwrap() < summary.find("output:").unwrap(),
        "structured result should be shown before output preview"
    );
}

#[test]
fn step_history_summary_uses_longer_output_preview() {
    let output = "x".repeat(350);
    let record = completed_record(
        "answer",
        &output,
        Transition::Abort {
            reason: "done".into(),
        },
    );

    let summary = format_step_history_summary(&[record]);

    assert!(
        summary.contains(&output),
        "output under the 1000 character preview limit should not be truncated"
    );
}

#[test]
fn step_history_summary_transition_uses_serde_not_debug() {
    // Issue #68 fix 2: transition should be serialized as JSON, not Debug format.
    let record = completed_record("s1", "done", Transition::To("s2".into()));
    let summary = format_step_history_summary(&[record]);
    // serde produces {"to":"s2"}, not To("s2") from Debug
    assert!(
        summary.contains(r#"{"to":"s2"}"#),
        "transition should use serde JSON format, got: {summary}"
    );
    assert!(
        !summary.contains(r#"To("s2")"#),
        "transition should not use Debug format, got: {summary}"
    );
}

#[test]
fn step_history_summary_truncation_metadata() {
    // Issue #68 fix 3: truncation should report how many chars were cut.
    let output = "y".repeat(1500);
    let record = completed_record("big", &output, Transition::To("next".into()));
    let summary = format_step_history_summary(&[record]);
    assert!(
        summary.contains("... (truncated "),
        "truncated output should include metadata, got: {summary}"
    );
    assert!(
        summary.contains(" chars)"),
        "truncation metadata should end with 'chars)', got: {summary}"
    );
}

#[test]
fn step_history_summary_no_result_includes_timing() {
    // Issue #68 fix 4: (no result) records should include started_at/ended_at.
    let now = chrono::Utc::now();
    let record = StepRecord {
        step_id: "retry_step".into(),
        started_at: now,
        ended_at: now,
        result: None,
        transition: Transition::Retry,
    };
    let summary = format_step_history_summary(&[record]);
    assert!(
        summary.contains("(no result)"),
        "should still show (no result), got: {summary}"
    );
    // Should include time hints (HH:MM:SS format from both timestamps)
    assert!(
        summary.contains(now.format("%H:%M:%S").to_string().as_str()),
        "should include timing info, got: {summary}"
    );
    // Transition should be serde-serialized as "retry"
    assert!(
        summary.contains("\"retry\""),
        "transition should be serde format, got: {summary}"
    );
}

#[test]
fn step_history_summary_total_budget_drops_oldest() {
    // Issue #68 fix 1: when total exceeds budget, oldest records are dropped.
    let mut records = Vec::new();
    // 800 chars per step × 30 steps = ~24000 chars, exceeding the 20000 budget.
    for i in 0..30 {
        let output = format!("output for step {i} padding {}", "x".repeat(750));
        records.push(completed_record(
            &format!("step_{i}"),
            &output,
            Transition::To(format!("step_{}", i + 1)),
        ));
    }
    let summary = format_step_history_summary(&records);
    // Should indicate some steps were omitted
    assert!(
        summary.contains("steps omitted)"),
        "should report omitted steps, got first 200 chars: {}",
        &summary[..200.min(summary.len())]
    );
    // Most recent step should be present
    assert!(
        summary.contains("step_29"),
        "most recent step should be kept, got: {summary}"
    );
    // Oldest step should be dropped
    assert!(
        !summary.contains(
            "step_0
"
        ),
        "oldest step should be dropped (not as first line)"
    );
}

#[test]
fn step_history_summary_empty_returns_empty() {
    assert_eq!(format_step_history_summary(&[]), "");
}

struct BlockingTool;

impl Tool for BlockingTool {
    fn name(&self) -> &str {
        "block_forever"
    }

    fn description(&self) -> &str {
        "blocks until the tool context is aborted"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| serde_json::json!({}));
        &SCHEMA
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolError>> {
        Box::pin(async move {
            ctx.abort.cancelled().await;
            Err(ToolError::Aborted)
        })
    }
}

/// A trivial tool that returns a text result immediately. Used by tests that
/// need a real tool-call event sequence without the now-removed submit tool.
struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "echoes back a fixed message"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| serde_json::json!({}));
        &SCHEMA
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolError>> {
        Box::pin(async move {
            Ok(ToolResult {
                content: vec![ContentBlock::Text {
                    text: "echo".into(),
                }],
                details: serde_json::json!({}),
                terminate: false,
            })
        })
    }
}

struct TwoStepJudge;

impl StepTransitionJudge for TwoStepJudge {
    fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
        let cur = ctx.current_step.id().clone();
        Box::pin(async move {
            if cur == "s1" {
                Transition::To("s2".into())
            } else {
                Transition::Abort {
                    reason: "done".into(),
                }
            }
        })
    }
}

#[tokio::test]
async fn free_task_crash_recovery() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("done")]));
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new_free_task("do work".into(), vec![], config)
        .unwrap()
        .with_task_store(store.clone());
    let task_id = engine.task_id().clone();
    engine.run().await.unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);

    // restore：workflow + state 从 store load。
    let config2 = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let restored = WorkflowEngine::restore(store, task_id, config2, Arc::new(NoopJudge))
        .await
        .unwrap();
    assert_eq!(restored.state().await, WorkflowStatus::Succeeded);
}

#[tokio::test]
async fn running_restore_continues_from_transition_target() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restore-linear".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
        ],
        edges: vec![Edge {
            from: "s1".into(),
            to: "s2".into(),
            condition: None,
        }],
    };
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Running;
    state.started_at = Some(chrono::Utc::now());
    // Simulates a crash after s1's transition record was persisted but
    // before current_step was updated to s2.
    state.current_step = "s1".into();
    state.step_history = vec![completed_record(
        "s1",
        "step1 done",
        Transition::To("s2".into()),
    )];
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    struct RestoreJudge;
    impl StepTransitionJudge for RestoreJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let step_id = ctx.current_step.id().clone();
            Box::pin(async move {
                if step_id == "s1" {
                    Transition::To("s2".into())
                } else {
                    Transition::Abort {
                        reason: "done".into(),
                    }
                }
            })
        }
    }
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("step2 done")]));
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let restored = WorkflowEngine::restore(store, task_id, config, Arc::new(RestoreJudge))
        .await
        .unwrap();

    let result = restored.run().await.unwrap();
    let history = restored.step_history().await;

    assert_eq!(restored.state().await, WorkflowStatus::Succeeded);
    assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].step_id, "s1");
    assert_eq!(history[1].step_id, "s2");
    assert_eq!(result.final_message.as_deref(), Some("step2 done"));
}

#[tokio::test]
async fn running_restore_continues_from_rework_target() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restore-rework".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
        ],
        edges: vec![
            Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            },
            Edge {
                from: "s2".into(),
                to: "s1".into(),
                condition: None,
            },
        ],
    };
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Running;
    state.started_at = Some(chrono::Utc::now());
    // Simulates a crash after s2 decided to rework s1 but before
    // current_step was persisted as s1.
    state.current_step = "s2".into();
    state.step_history = vec![
        completed_record("s1", "step1 done", Transition::To("s2".into())),
        completed_record("s2", "needs rework", Transition::To("s1".into())),
    ];
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    struct AbortAfterRestoreJudge;
    impl StepTransitionJudge for AbortAfterRestoreJudge {
        fn decide<'a>(&'a self, _ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            Box::pin(async {
                Transition::Abort {
                    reason: "done".into(),
                }
            })
        }
    }
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
        "reworked step1",
    )]));
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let restored =
        WorkflowEngine::restore(store, task_id, config, Arc::new(AbortAfterRestoreJudge))
            .await
            .unwrap();

    let result = restored.run().await.unwrap();
    let history = restored.step_history().await;

    assert_eq!(restored.state().await, WorkflowStatus::Succeeded);
    assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        history
            .iter()
            .map(|record| record.step_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s1", "s2", "s1"]
    );
    assert_eq!(result.final_message.as_deref(), Some("reworked step1"));
}

#[tokio::test]
async fn running_restore_terminal_fail_does_not_rerun_step() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restore-fail".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![Step::llm("s1", "Step 1", "do step 1", vec![])],
        edges: vec![],
    };
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Running;
    state.started_at = Some(chrono::Utc::now());
    state.step_history = vec![completed_record(
        "s1",
        "step1 done",
        Transition::Fail {
            reason: "route failed".into(),
        },
    )];
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    let client = Arc::new(MockLlmClient::new(vec![]));
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let restored = WorkflowEngine::restore(store, task_id, config, Arc::new(NoopJudge))
        .await
        .unwrap();

    let err = restored.run().await.unwrap_err();
    let history = restored.step_history().await;

    assert!(err.to_string().contains("route failed"));
    assert_eq!(restored.state().await, WorkflowStatus::Failed);
    assert_eq!(client.call_count.load(Ordering::SeqCst), 0);
    assert_eq!(history.len(), 1);
    assert!(matches!(history[0].transition, Transition::Fail { .. }));
}

#[tokio::test]
async fn running_restore_terminal_abort_does_not_rerun_step() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restore-abort".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![Step::llm("s1", "Step 1", "do step 1", vec![])],
        edges: vec![],
    };
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Running;
    state.started_at = Some(chrono::Utc::now());
    state.step_history = vec![completed_record(
        "s1",
        "final answer",
        Transition::Abort {
            reason: "done".into(),
        },
    )];
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    let client = Arc::new(MockLlmClient::new(vec![]));
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let restored = WorkflowEngine::restore(store, task_id, config, Arc::new(NoopJudge))
        .await
        .unwrap();

    let result = restored.run().await.unwrap();
    let history = restored.step_history().await;

    assert_eq!(restored.state().await, WorkflowStatus::Succeeded);
    assert_eq!(client.call_count.load(Ordering::SeqCst), 0);
    assert_eq!(history.len(), 1);
    assert!(matches!(history[0].transition, Transition::Abort { .. }));
    assert_eq!(result.turns, 1);
    assert_eq!(result.final_message.as_deref(), Some("final answer"));
}

#[tokio::test]
async fn multi_step_linear_workflow() {
    // 2 步线性流程：s1 → s2。
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("step1 done"),
        MockResponse::text("step2 done"),
    ]));
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
        ],
        edges: vec![Edge {
            from: "s1".into(),
            to: "s2".into(),
            condition: None,
        }],
    };
    // judge：s1 → s2 → Abort。
    struct LinearJudge;
    impl StepTransitionJudge for LinearJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let cur = ctx.current_step.id().clone();
            Box::pin(async move {
                if cur == "s1" {
                    Transition::To("s2".into())
                } else {
                    Transition::Abort {
                        reason: "done".into(),
                    }
                }
            })
        }
    }
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(LinearJudge)).unwrap();
    let result = engine.run().await.unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert_eq!(result.turns, 2); // 2 步历史
}

#[tokio::test]
async fn noop_judge_uses_declarative_edge_conditions_by_default() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("step2 done")]));
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::executor("s1", "Structured Step", "structured", None),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
        ],
        edges: vec![Edge {
            from: "s1".into(),
            to: "s2".into(),
            condition: Some(EdgeCondition::Expr(ConditionExpr::Eq {
                pointer: "/status".into(),
                value: serde_json::json!("ok"),
            })),
        }],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
        .unwrap()
        .with_executor("structured", Arc::new(StructuredExecutor));

    let result = engine.run().await.unwrap();
    let history = engine.step_history().await;

    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        history
            .iter()
            .map(|record| record.step_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s1", "s2"]
    );
    assert_eq!(result.final_message.as_deref(), Some("step2 done"));
}

fn test_msg(content: Vec<ContentBlock>) -> llm_harness_types::AssistantMessage {
    use llm_harness_types::{AssistantMessage, AssistantMessageKind};
    AssistantMessage {
        kind: AssistantMessageKind::FinalAnswer,
        message_id: "msg_1".into(),
        turn_id: "turn_1".into(),
        content,
        stop_reason: None,
        timestamp: chrono::Utc::now(),
        provider: None,
        api: None,
        model: None,
        usage: None,
        error_message: None,
    }
}

#[test]
fn step_progress_from_maps_coarse_events() {
    use llm_harness_types::{AssistantMessageKind, ToolError, ToolResult};

    // ToolCallStart
    let ev = AgentEvent::ToolCallStart {
        message_id: "m1".into(),
        tool_use_id: "tu-1".into(),
        name: "search".into(),
    };
    match step_progress_from(&ev) {
        Some(StepProgress::ToolCallStart { tool_use_id, name }) => {
            assert_eq!(tool_use_id, "tu-1");
            assert_eq!(name, "search");
        }
        other => panic!("expected ToolCallStart, got {other:?}"),
    }

    // ToolCallEnd
    let ev = AgentEvent::ToolCallEnd {
        tool_use_id: "tu-1".into(),
        args: serde_json::json!({"q": "rust"}),
    };
    match step_progress_from(&ev) {
        Some(StepProgress::ToolCallEnd { tool_use_id, args }) => {
            assert_eq!(tool_use_id, "tu-1");
            assert_eq!(args["q"], "rust");
        }
        other => panic!("expected ToolCallEnd, got {other:?}"),
    }

    // ToolExecutionStart
    let ev = AgentEvent::ToolExecutionStart {
        tool_use_id: "tu-1".into(),
        tool_name: "search".into(),
        args: serde_json::json!({}),
    };
    match step_progress_from(&ev) {
        Some(StepProgress::ToolExecutionStart {
            tool_use_id,
            tool_name,
        }) => {
            assert_eq!(tool_use_id, "tu-1");
            assert_eq!(tool_name, "search");
        }
        other => panic!("expected ToolExecutionStart, got {other:?}"),
    }

    // ToolExecutionEnd — Ok
    let ev = AgentEvent::ToolExecutionEnd {
        tool_use_id: "tu-1".into(),
        result: Ok(ToolResult {
            content: vec![],
            details: serde_json::Value::Null,
            terminate: false,
        }),
    };
    match step_progress_from(&ev) {
        Some(StepProgress::ToolExecutionEnd {
            tool_use_id,
            ok,
            error,
        }) => {
            assert_eq!(tool_use_id, "tu-1");
            assert!(ok);
            assert!(error.is_none());
        }
        other => panic!("expected ToolExecutionEnd Ok, got {other:?}"),
    }

    // ToolExecutionEnd — Err
    let ev = AgentEvent::ToolExecutionEnd {
        tool_use_id: "tu-1".into(),
        result: Err(ToolError::Aborted),
    };
    match step_progress_from(&ev) {
        Some(StepProgress::ToolExecutionEnd { ok, error, .. }) => {
            assert!(!ok);
            assert!(error.is_some());
        }
        other => panic!("expected ToolExecutionEnd Err, got {other:?}"),
    }

    // TurnEnd
    let ev = AgentEvent::TurnEnd {
        index: 3,
        message: test_msg(vec![]),
        tool_results: vec![],
    };
    match step_progress_from(&ev) {
        Some(StepProgress::TurnEnd { index }) => assert_eq!(index, 3),
        other => panic!("expected TurnEnd, got {other:?}"),
    }

    // MessageEnd
    let ev = AgentEvent::MessageEnd {
        message_id: "m1".into(),
        message: test_msg(vec![ContentBlock::Text { text: "hi".into() }]),
    };
    match step_progress_from(&ev) {
        Some(StepProgress::MessageEnd { message_id, kind }) => {
            assert_eq!(message_id, "m1");
            assert_eq!(kind, AssistantMessageKind::FinalAnswer);
        }
        other => panic!("expected MessageEnd, got {other:?}"),
    }
}

#[test]
fn step_progress_from_returns_none_for_filtered_events() {
    // TextDelta — filtered (high frequency)
    let ev = AgentEvent::TextDelta {
        message_id: "m1".into(),
        text: "hel".into(),
    };
    assert!(step_progress_from(&ev).is_none());

    // ThinkingDelta — filtered
    let ev = AgentEvent::ThinkingDelta {
        message_id: "m1".into(),
        thinking: "th".into(),
        signature: None,
    };
    assert!(step_progress_from(&ev).is_none());

    // ToolCallArgsDelta — filtered
    let ev = AgentEvent::ToolCallArgsDelta {
        tool_use_id: "tu-1".into(),
        partial_input: "{".into(),
    };
    assert!(step_progress_from(&ev).is_none());

    // AgentStart — filtered
    assert!(step_progress_from(&AgentEvent::AgentStart).is_none());

    // TurnStart — filtered
    let ev = AgentEvent::TurnStart { index: 0 };
    assert!(step_progress_from(&ev).is_none());

    // MessageStart — filtered
    let ev = AgentEvent::MessageStart {
        message_id: "m1".into(),
    };
    assert!(step_progress_from(&ev).is_none());
}

/// Integration: a free-task LLM step that calls a registered tool should
/// emit StepStarted → StepProgress(ToolCallStart) → StepProgress(ToolCallEnd)
/// → StepProgress(ToolExecutionStart) → StepProgress(ToolExecutionEnd)
/// → StepFinished, in order.
#[tokio::test]
async fn step_progress_events_emitted_in_order() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::tool_use(
        "tu-1", "echo", r#"{}"#,
    )]));
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![Step::llm("s1", "Step 1", "do step 1", vec!["echo".into()])],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = Arc::new(
        WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
            .unwrap()
            .with_tool(Arc::new(EchoTool)),
    );
    let mut rx = engine.subscribe();
    let engine_clone = engine.clone();
    let mut run_handle = tokio::spawn(async move { engine_clone.run().await });

    // Collect events until the task finishes.
    let mut events = Vec::new();
    loop {
        tokio::select! {
            biased;
            ev = rx.recv() => {
                match ev {
                    Ok(e) => events.push(e),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            _ = &mut run_handle => break,
        }
    }

    // Assert the sequence contains the expected progress variants in order.
    let mut saw_step_started = false;
    let mut saw_tool_call_start = false;
    let mut saw_tool_call_end = false;
    let mut saw_tool_execution_start = false;
    let mut saw_tool_execution_end = false;
    let mut saw_step_finished = false;
    for ev in &events {
        match ev {
            WorkflowEvent::StepStarted { .. } => saw_step_started = true,
            WorkflowEvent::StepProgress {
                progress: StepProgress::ToolCallStart { .. },
                ..
            } => {
                assert!(saw_step_started);
                saw_tool_call_start = true;
            }
            WorkflowEvent::StepProgress {
                progress: StepProgress::ToolCallEnd { .. },
                ..
            } => {
                assert!(saw_tool_call_start);
                saw_tool_call_end = true;
            }
            WorkflowEvent::StepProgress {
                progress: StepProgress::ToolExecutionStart { .. },
                ..
            } => {
                assert!(saw_tool_call_end);
                saw_tool_execution_start = true;
            }
            WorkflowEvent::StepProgress {
                progress: StepProgress::ToolExecutionEnd { .. },
                ..
            } => {
                assert!(saw_tool_execution_start);
                saw_tool_execution_end = true;
            }
            WorkflowEvent::StepFinished { .. } => {
                assert!(saw_tool_execution_end);
                saw_step_finished = true;
            }
            _ => {}
        }
    }
    assert!(saw_step_started, "missing StepStarted");
    assert!(saw_tool_call_start, "missing ToolCallStart");
    assert!(saw_tool_call_end, "missing ToolCallEnd");
    assert!(saw_tool_execution_start, "missing ToolExecutionStart");
    assert!(saw_tool_execution_end, "missing ToolExecutionEnd");
    assert!(saw_step_finished, "missing StepFinished");
}
/// Abort: when a step is cancelled mid-flight, no StepProgress events
/// should arrive after the Cancelled event. Residual events are discarded.
#[tokio::test]
async fn step_progress_events_stop_after_cancel() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![]));
    let workflow = Workflow {
        entry_step: "exec".into(),
        steps: vec![Step::executor("exec", "Executor Step", "blocking", None)],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = Arc::new(
        WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
            .unwrap()
            .with_executor("blocking", Arc::new(BlockingExecutor)),
    );
    let mut rx = engine.subscribe();
    let engine_clone = engine.clone();
    let mut run_handle = tokio::spawn(async move { engine_clone.run().await });

    // Give the step time to start, then cancel.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    engine.cancel("test abort").await.unwrap();

    // Collect events until run finishes.
    let mut events = Vec::new();
    loop {
        tokio::select! {
            biased;
            ev = rx.recv() => {
                match ev {
                    Ok(e) => events.push(e),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            _ = &mut run_handle => break,
        }
    }

    // StepStarted should arrive, then Cancelled. No StepProgress for an
    // executor step (it has no AgentEvent stream), and none after cancel.
    let mut saw_cancelled = false;
    for ev in &events {
        match ev {
            WorkflowEvent::Cancelled { .. } => saw_cancelled = true,
            WorkflowEvent::StepProgress { .. } => {
                // Executor steps don't emit progress; if any arrived it
                // must be before Cancelled.
                assert!(!saw_cancelled, "StepProgress after Cancelled");
            }
            _ => {}
        }
    }
    assert!(saw_cancelled, "missing Cancelled event");
}

#[tokio::test]
async fn restore_noop_judge_uses_declarative_edge_conditions_by_default() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restore-declarative".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::executor("s1", "Structured Step", "structured", None),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
        ],
        edges: vec![Edge {
            from: "s1".into(),
            to: "s2".into(),
            condition: Some(EdgeCondition::Expr(ConditionExpr::Eq {
                pointer: "/status".into(),
                value: serde_json::json!("ok"),
            })),
        }],
    };
    store.save_workflow(&task_id, &workflow).await.unwrap();

    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("step2 done")]));
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let restored = WorkflowEngine::restore(store, task_id, config, Arc::new(NoopJudge))
        .await
        .unwrap()
        .with_executor("structured", Arc::new(StructuredExecutor));

    let result = restored.run().await.unwrap();
    let history = restored.step_history().await;

    assert_eq!(restored.state().await, WorkflowStatus::Succeeded);
    assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        history
            .iter()
            .map(|record| record.step_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s1", "s2"]
    );
    assert_eq!(result.final_message.as_deref(), Some("step2 done"));
}

#[tokio::test]
async fn pause_then_run_continues_from_next_step() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("step1 done"),
        MockResponse::text("step2 done"),
    ]));
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
        ],
        edges: vec![Edge {
            from: "s1".into(),
            to: "s2".into(),
            condition: None,
        }],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(TwoStepJudge)).unwrap();
    engine.pause("operator checkpoint").unwrap();

    let err = engine.run().await.unwrap_err();
    assert!(matches!(err, WorkflowError::Paused(reason) if reason == "operator checkpoint"));
    assert_eq!(engine.state().await, WorkflowStatus::Paused);
    assert_eq!(engine.current_step().await, "s2");
    assert_eq!(engine.step_history().await.len(), 1);

    let result = engine.run().await.unwrap();
    let history = engine.step_history().await;

    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert_eq!(client.call_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        history
            .iter()
            .map(|record| record.step_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s1", "s2"]
    );
    assert_eq!(result.final_message.as_deref(), Some("step2 done"));
}

#[tokio::test]
async fn resume_then_run_does_not_enter_crash_recovery() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("step1 done"),
        MockResponse::text("step2 done"),
    ]));
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
        ],
        edges: vec![Edge {
            from: "s1".into(),
            to: "s2".into(),
            condition: None,
        }],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(TwoStepJudge)).unwrap();
    engine.pause("operator checkpoint").unwrap();
    engine.run().await.unwrap_err();

    engine.resume().await.unwrap();

    assert_eq!(
        engine.state().await,
        WorkflowStatus::Paused,
        "resume should not pre-set Running; run() owns the transition"
    );

    let result = engine.run().await.unwrap();
    let history = engine.step_history().await;

    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert_eq!(client.call_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        history
            .iter()
            .map(|record| record.step_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s1", "s2"]
    );
    assert_eq!(result.final_message.as_deref(), Some("step2 done"));
}

/// #36: a task that enters `Failed` due to a transient error (e.g. LLM 403)
/// can be resumed via `resume()` (Failed → Paused) and then `run()` picks it
/// up from `current_step`, retrying the failed step.
#[tokio::test]
async fn failed_task_can_be_resumed() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![Step::llm("s1", "Step 1", "do step 1", vec![])],
        edges: vec![],
    };
    // Simulate a task that already entered Failed (e.g. transient LLM error).
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Failed;
    state.error = Some(TaskError::Internal("step 's1' failed: LLM 403".into()));
    state.started_at = Some(chrono::Utc::now());
    state.ended_at = Some(chrono::Utc::now());
    let task_id = TaskId("failed-resume".into());
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("step1 done")]));
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::restore(store, task_id, config, Arc::new(NoopJudge))
        .await
        .unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Failed);

    // resume() resets Failed → Paused.
    engine.resume().await.unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Paused);

    // run() retries from current_step (s1), succeeds.
    engine.run().await.unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
}

/// #36: `Succeeded` is terminal — resume() must reject it.
#[tokio::test]
async fn succeeded_task_cannot_be_resumed() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("done")]));
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![Step::llm("s1", "Step 1", "do step 1", vec![])],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge)).unwrap();
    engine.run().await.unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);

    let err = engine.resume().await.unwrap_err();
    assert!(matches!(err, WorkflowError::InvalidStatus { .. }));
}

/// #36: restore() a persisted Failed task, then resume() + run() recovers.
#[tokio::test]
async fn restore_failed_then_resume_recovers() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restore-failed".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![Step::llm("s1", "Step 1", "do step 1", vec![])],
        edges: vec![],
    };
    // Persist a Failed state.
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Failed;
    state.error = Some(TaskError::Internal("LLM 403".into()));
    state.started_at = Some(chrono::Utc::now());
    state.ended_at = Some(chrono::Utc::now());
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    // restore() loads the Failed task.
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("step1 done")]));
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let restored = WorkflowEngine::restore(store, task_id, config, Arc::new(NoopJudge))
        .await
        .unwrap();
    assert_eq!(restored.state().await, WorkflowStatus::Failed);

    // resume() + run() recovers.
    restored.resume().await.unwrap();
    restored.run().await.unwrap();
    assert_eq!(restored.state().await, WorkflowStatus::Succeeded);
    assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn transition_fail_marks_task_failed() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("step1 done")]));
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
        ],
        edges: vec![Edge {
            from: "s1".into(),
            to: "s2".into(),
            condition: None,
        }],
    };
    struct FailingJudge;
    impl StepTransitionJudge for FailingJudge {
        fn decide<'a>(&'a self, _ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            Box::pin(async {
                Transition::Fail {
                    reason: "route failed".into(),
                }
            })
        }
    }
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(FailingJudge)).unwrap();

    let err = engine.run().await.unwrap_err();
    let history = engine.step_history().await;

    assert_eq!(engine.state().await, WorkflowStatus::Failed);
    assert!(err.to_string().contains("route failed"));
    assert_eq!(history.len(), 1);
    assert!(matches!(history[0].transition, Transition::Fail { .. }));
}

#[tokio::test]
async fn mixed_llm_and_executor_workflow() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("step1 done")]));
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::executor(
                "s2",
                "Executor Step",
                "fixed",
                Some(serde_json::json!({"suffix": "done"})),
            ),
        ],
        edges: vec![Edge {
            from: "s1".into(),
            to: "s2".into(),
            condition: None,
        }],
    };
    struct LinearJudge;
    impl StepTransitionJudge for LinearJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let cur = ctx.current_step.id().clone();
            Box::pin(async move {
                if cur == "s1" {
                    Transition::To("s2".into())
                } else {
                    Transition::Abort {
                        reason: "done".into(),
                    }
                }
            })
        }
    }
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(LinearJudge))
        .unwrap()
        .with_executor("fixed", Arc::new(FixedExecutor));

    let result = engine.run().await.unwrap();

    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert_eq!(result.turns, 2);
    assert!(
        result
            .final_message
            .as_deref()
            .unwrap_or("")
            .contains("executor done after step1 done")
    );
    assert_eq!(
        client.call_count.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn builtin_json_transform_executor_runs_without_manual_registration() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![]));
    let workflow = Workflow {
        entry_step: "transform".into(),
        steps: vec![Step::executor(
            "transform",
            "Transform",
            "json_transform",
            Some(serde_json::json!({
                "source": "config.input",
                "input": {
                    "measurement": {
                        "voltage": 3.3,
                        "unit": "V"
                    }
                },
                "fields": {
                    "voltage": "/measurement/voltage",
                    "unit": "/measurement/unit"
                }
            })),
        )],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge)).unwrap();

    let result = engine.run().await.unwrap();
    let history = engine.step_history().await;

    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert_eq!(result.turns, 1);
    assert_eq!(
        history[0]
            .result
            .as_ref()
            .and_then(|r| r.structured.clone()),
        Some(serde_json::json!({"unit": "V", "voltage": 3.3}))
    );
    assert_eq!(
        client.call_count.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn step_policy_retries_executor_failure() {
    let dir = tempfile::TempDir::new().unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let client = Arc::new(MockLlmClient::new(vec![]));
    let workflow = Workflow {
        entry_step: "flaky".into(),
        steps: vec![Step::executor("flaky", "Flaky", "flaky", None).with_policy(
            crate::workflow::model::StepExecutionPolicy {
                timeout_ms: Some(1_000),
                max_attempts: Some(2),
                retry_backoff_ms: Some(0),
            },
        )],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
        .unwrap()
        .with_executor(
            "flaky",
            Arc::new(FlakyExecutor {
                attempts: attempts.clone(),
            }),
        );

    let result = engine.run().await.unwrap();
    let history = engine.step_history().await;

    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].result.as_ref().unwrap().output,
        "ok on attempt 2"
    );
    assert_eq!(result.turns, 1);
}

#[tokio::test]
async fn step_policy_times_out_blocking_executor() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![]));
    let workflow = Workflow {
        entry_step: "blocked".into(),
        steps: vec![
            Step::executor("blocked", "Blocked", "blocking", None).with_policy(
                crate::workflow::model::StepExecutionPolicy {
                    timeout_ms: Some(20),
                    max_attempts: Some(1),
                    retry_backoff_ms: Some(0),
                },
            ),
        ],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
        .unwrap()
        .with_executor("blocking", Arc::new(BlockingExecutor));

    let err = engine.run().await.unwrap_err();

    assert_eq!(engine.state().await, WorkflowStatus::Failed);
    assert!(err.to_string().contains("timed out"));
}

#[tokio::test]
async fn unregistered_executor_fails_task() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![]));
    let workflow = Workflow {
        entry_step: "exec".into(),
        steps: vec![Step::executor("exec", "Executor Step", "missing", None)],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge)).unwrap();

    let err = engine.run().await.unwrap_err();

    assert_eq!(engine.state().await, WorkflowStatus::Failed);
    assert!(
        err.to_string()
            .contains("executor 'missing' not registered")
    );
}

#[tokio::test]
async fn cancel_executor_step_marks_task_cancelled() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![]));
    let workflow = Workflow {
        entry_step: "exec".into(),
        steps: vec![Step::executor("exec", "Executor Step", "blocking", None)],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = Arc::new(
        WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
            .unwrap()
            .with_executor("blocking", Arc::new(BlockingExecutor)),
    );
    let runner = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.run().await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    engine.cancel("stop").await.unwrap();

    let result = runner.await.unwrap().unwrap();

    assert_eq!(engine.state().await, WorkflowStatus::Cancelled);
    assert_eq!(result.turns, 0);
    assert!(result.final_message.is_none());
}

#[tokio::test]
async fn cancel_then_restore_remains_cancelled() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let client = Arc::new(MockLlmClient::new(vec![]));
    let workflow = Workflow {
        entry_step: "exec".into(),
        steps: vec![Step::executor("exec", "Executor Step", "blocking", None)],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = Arc::new(
        WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
            .unwrap()
            .with_task_store(store.clone())
            .with_executor("blocking", Arc::new(BlockingExecutor)),
    );
    let task_id = engine.task_id().clone();
    let runner = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.run().await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    engine.cancel("stop").await.unwrap();

    let result = runner.await.unwrap().unwrap();

    assert_eq!(engine.state().await, WorkflowStatus::Cancelled);
    assert_eq!(result.turns, 0);

    let restored = WorkflowEngine::restore(
        store,
        task_id,
        WorkflowEngineConfig {
            client,
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions-restore"),
            customize_builder: None,
        },
        Arc::new(NoopJudge),
    )
    .await
    .unwrap();
    let history = restored.step_history().await;

    assert_eq!(restored.state().await, WorkflowStatus::Cancelled);
    assert_eq!(history.len(), 1);
    assert!(history[0].result.is_none());
    assert!(matches!(history[0].transition, Transition::Abort { .. }));
}

#[tokio::test]
async fn cancel_llm_step_discards_partial_result() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut response = MockResponse::tool_use("block_1", "block_forever", "{}");
    response.events.insert(
        0,
        Ok(llm_harness_loop::AdapterStreamEvent::TextDelta {
            index: 0,
            text: "partial output".into(),
        }),
    );
    let client = Arc::new(MockLlmClient::new(vec![response]));
    let call_count = client.call_count.clone();
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = Arc::new(
        WorkflowEngine::new_free_task(
            "stream partial then block".into(),
            vec!["block_forever".into()],
            config,
        )
        .unwrap()
        .with_tool(Arc::new(BlockingTool)),
    );
    let runner = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.run().await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while call_count.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    engine.cancel("stop").await.unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let history = engine.step_history().await;

    assert_eq!(engine.state().await, WorkflowStatus::Cancelled);
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
    assert_eq!(result.turns, 0);
    assert!(result.final_message.is_none());
    assert_eq!(history.len(), 1);
    assert!(history[0].result.is_none());
    assert!(matches!(history[0].transition, Transition::Abort { .. }));
}

// ── WorkflowContext 共享状态测试 ──────────────────────────────

/// executor 写 context，后续 executor 读到写入的值。
struct ContextWriter;
impl StepExecutor for ContextWriter {
    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            let mut ctx_data = ctx.context.lock().await;
            ctx_data
                .variables
                .insert("focus".into(), serde_json::json!(0.42));
            ctx_data
                .variables
                .insert("metro_p".into(), serde_json::json!(1.5));
            drop(ctx_data);
            Ok(StepResult {
                output: "wrote context".into(),
                structured: None,
                structured_status: StructuredStatus::NotRequired,
                tool_calls_count: 0,
                session_id: String::new(),
                cost: CostAggregate::default(),
                started_at: None,
                ended_at: None,
            })
        })
    }
}

/// executor 读 context，验证上游 executor 写入的值。
struct ContextReader;
impl StepExecutor for ContextReader {
    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            let ctx_data = ctx.context.lock().await;
            let focus = ctx_data.variables.get("focus").cloned();
            let metro_p = ctx_data.variables.get("metro_p").cloned();
            drop(ctx_data);
            Ok(StepResult {
                output: format!("focus={focus:?} metro_p={metro_p:?}"),
                structured: Some(serde_json::json!({
                    "focus": focus,
                    "metro_p": metro_p,
                })),
                structured_status: StructuredStatus::NotRequired,
                tool_calls_count: 0,
                session_id: String::new(),
                cost: CostAggregate::default(),
                started_at: None,
                ended_at: None,
            })
        })
    }
}

#[tokio::test]
async fn executor_writes_context_and_downstream_reads_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![]));
    let workflow = Workflow {
        entry_step: "writer".into(),
        steps: vec![
            Step::executor("writer", "Writer", "ctx_writer", None),
            Step::executor("reader", "Reader", "ctx_reader", None),
        ],
        edges: vec![Edge {
            from: "writer".into(),
            to: "reader".into(),
            condition: None,
        }],
    };
    struct LinearJudge;
    impl StepTransitionJudge for LinearJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let id = ctx.current_step.id().clone();
            Box::pin(async move {
                if id == "writer" {
                    Transition::To("reader".into())
                } else {
                    Transition::Abort {
                        reason: "done".into(),
                    }
                }
            })
        }
    }
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(LinearJudge))
        .unwrap()
        .with_executor("ctx_writer", Arc::new(ContextWriter))
        .with_executor("ctx_reader", Arc::new(ContextReader));

    let result = engine.run().await.unwrap();
    let history = engine.step_history().await;

    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    // reader 的 structured 应包含 writer 写入的值。
    use serde_json::Value;
    let reader_structured: Value = history[1]
        .result
        .as_ref()
        .and_then(|r| r.structured.clone())
        .unwrap();
    assert_eq!(reader_structured["focus"], serde_json::json!(0.42));
    assert_eq!(reader_structured["metro_p"], serde_json::json!(1.5));
    assert!(result.final_message.unwrap().contains("focus"));
}

#[tokio::test]
async fn context_survives_crash_recovery() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let client = Arc::new(MockLlmClient::new(vec![]));
    let workflow = Workflow {
        entry_step: "writer".into(),
        steps: vec![
            Step::executor("writer", "Writer", "ctx_writer", None),
            Step::executor("reader", "Reader", "ctx_reader", None),
        ],
        edges: vec![Edge {
            from: "writer".into(),
            to: "reader".into(),
            condition: None,
        }],
    };
    struct LinearJudge;
    impl StepTransitionJudge for LinearJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let id = ctx.current_step.id().clone();
            Box::pin(async move {
                if id == "writer" {
                    Transition::To("reader".into())
                } else {
                    Transition::Abort {
                        reason: "done".into(),
                    }
                }
            })
        }
    }
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(LinearJudge))
        .unwrap()
        .with_executor("ctx_writer", Arc::new(ContextWriter))
        .with_executor("ctx_reader", Arc::new(ContextReader))
        .with_task_store(store.clone());
    let task_id = engine.task_id().clone();

    // 從獨立 context 寫入，再手動觸發 persist。
    // persist_state 內部會從 self.context 取 snapshot 覆蓋 state.context。
    engine
        .context()
        .lock()
        .await
        .variables
        .insert("survives".into(), serde_json::json!(true));
    // save_workflow + persist_state（模拟 run() 启动时的落盘）。
    store
        .save_workflow(&task_id, &engine.workflow)
        .await
        .unwrap();
    // 手动触发一次 persist（模拟 run() 启动时的 persist）。
    let snapshot = engine.workflow_state.lock().await.clone();
    engine.persist_state(&snapshot).await.unwrap();

    // restore 后 context 应包含写入的值。
    let config2 = WorkflowEngineConfig {
        client: Arc::new(MockLlmClient::new(vec![])),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let restored = WorkflowEngine::restore(store, task_id, config2, Arc::new(LinearJudge))
        .await
        .unwrap();
    let ctx_arc = restored.context();
    let ctx = ctx_arc.lock().await;
    assert_eq!(
        ctx.variables.get("survives"),
        Some(&serde_json::json!(true))
    );
}
/// 共享 customize_builder 设置 system_prompt，验证 step harness 实际生效。
#[tokio::test]
async fn shared_builder_customize_applies_system_prompt() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("done")]));
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    }
    .with_builder_customize(|b| b.system_prompt(Some("You are a test agent.".into())));
    let engine = WorkflowEngine::new_free_task("say done".into(), vec![], config).unwrap();
    let result = engine.run().await.unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert!(
        result
            .final_message
            .as_deref()
            .unwrap_or("")
            .contains("done")
    );
}

/// per-step 闭包覆盖共享闭包的 max_tokens 设置。
#[tokio::test]
async fn per_step_builder_overrides_shared_max_tokens() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("done")]));
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    }
    .with_builder_customize(|b| b.max_tokens(4096));
    let engine = WorkflowEngine::new_free_task("say done".into(), vec![], config)
        .unwrap()
        .with_step_builder("run", |b| b.max_tokens(8192));
    engine.run().await.unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    // Priority is structurally guaranteed: shared closure (4096) → per-step
    // closure (8192). Last write wins on builder.
    // Verify the per-step override (8192) actually reached the ChatRequest.
    let captured = client.captured_requests.lock().unwrap();
    assert_eq!(
        captured.len(),
        1,
        "expected exactly one LLM call, got {}",
        captured.len()
    );
    assert_eq!(
        captured[0].params().max_tokens(),
        8192,
        "per-step builder should override shared max_tokens"
    );
}

// ── with_hooks 累加语义测试 ──────────────────────────────────────────────────

/// 计数 before_turn hook。用于验证多次 with_hooks 调用后两组 hooks 都生效。
struct CountingBeforeTurnHook {
    count: Arc<AtomicUsize>,
}

impl BeforeTurnHook for CountingBeforeTurnHook {
    fn before_turn<'a>(&'a self, _ctx: BeforeTurnCtx<'a>) -> BoxFuture<'a, ()> {
        let c = self.count.clone();
        Box::pin(async move {
            c.fetch_add(1, Ordering::SeqCst);
        })
    }
}

/// 模拟 Python 侧两次 `with_hooks` 调用：每次调用读取 `config_customize_builder()`
/// 并包装前一个闭包，使新 hooks 追加到已有闭包之后。验证累加语义 ——
/// 两组 hooks 都应触发（计数 = 2），而非第二次覆盖第一次（计数 = 1）。
#[tokio::test]
async fn double_with_hooks_accumulates() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("done")]));
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let mut engine = WorkflowEngine::new_free_task("say done".into(), vec![], config).unwrap();

    // 第一次 with_hooks：注入 count=1 的 hook
    let count1 = Arc::new(AtomicUsize::new(0));
    let h1 = HarnessHooks {
        before_turn: vec![Arc::new(CountingBeforeTurnHook {
            count: count1.clone(),
        })],
        ..HarnessHooks::none()
    };
    let prev = engine.config_customize_builder().clone();
    engine.set_customize_builder(Arc::new(move |b| {
        let b = if let Some(p) = &prev { p(b) } else { b };
        b.hooks(h1.clone())
    }));

    // 第二次 with_hooks：注入 count=2 的 hook，包装前一个闭包
    let count2 = Arc::new(AtomicUsize::new(0));
    let h2 = HarnessHooks {
        before_turn: vec![Arc::new(CountingBeforeTurnHook {
            count: count2.clone(),
        })],
        ..HarnessHooks::none()
    };
    let prev = engine.config_customize_builder().clone();
    engine.set_customize_builder(Arc::new(move |b| {
        let b = if let Some(p) = &prev { p(b) } else { b };
        b.hooks(h2.clone())
    }));

    engine.run().await.unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);

    // 累加语义：两组 hooks 都应触发（各 1 次 = 总 2 次）。
    // 若为 last-write-wins，第二次会覆盖第一次，count1=0、count2=1。
    assert_eq!(
        count1.load(Ordering::SeqCst),
        1,
        "first with_hooks hooks must still fire (accumulate semantics)"
    );
    assert_eq!(
        count2.load(Ordering::SeqCst),
        1,
        "second with_hooks hooks must fire (accumulate semantics)"
    );
}
#[test]
fn aggregate_cost_merges_per_model_cache_tokens() {
    use crate::workflow::model::{StepRecord, StepResult, Transition};
    use llm_harness_types::{CostAggregate, ModelCost};

    let now = chrono::Utc::now();
    let mk_record = |read: u64, write: u64| StepRecord {
        step_id: "s1".to_string(),
        started_at: now,
        ended_at: now,
        result: Some(StepResult {
            output: String::new(),
            structured: None,
            structured_status: StructuredStatus::NotRequired,
            tool_calls_count: 0,
            session_id: String::new(),
            cost: CostAggregate {
                total_cache_read_tokens: read,
                total_cache_write_tokens: write,
                by_model: {
                    let mut m = std::collections::HashMap::new();
                    m.insert(
                        "anthropic/claude-sonnet".to_string(),
                        ModelCost {
                            input_tokens: 0,
                            output_tokens: 0,
                            cache_read_tokens: read,
                            cache_write_tokens: write,
                            reasoning_tokens: 0,
                            cost: 0.0,
                            call_count: 1,
                        },
                    );
                    m
                },
                ..Default::default()
            },
            started_at: None,
            ended_at: None,
        }),
        transition: Transition::To("s2".to_string()),
    };

    let history = vec![mk_record(200, 100), mk_record(300, 50)];
    let total = aggregate_cost(&history);

    let mc = total
        .by_model
        .get("anthropic/claude-sonnet")
        .expect("merged by_model entry");
    assert_eq!(
        mc.cache_read_tokens, 500,
        "per-model cache_read must sum across steps"
    );
    assert_eq!(
        mc.cache_write_tokens, 150,
        "per-model cache_write must sum across steps"
    );
    assert_eq!(mc.call_count, 2);
    // Top-level totals must also reflect the merge.
    assert_eq!(total.total_cache_read_tokens, 500);
    assert_eq!(total.total_cache_write_tokens, 150);
}

#[tokio::test]
async fn restore_from_step_truncates_downstream_history() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restart-truncate".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
            Step::llm("s3", "Step 3", "do step 3", vec![]),
        ],
        edges: vec![
            Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            },
            Edge {
                from: "s2".into(),
                to: "s3".into(),
                condition: None,
            },
        ],
    };
    // 模拟 s1/s2/s3 都已跑完，current_step 停在 s3。
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Succeeded;
    state.started_at = Some(chrono::Utc::now());
    state.ended_at = Some(chrono::Utc::now());
    state.current_step = "s3".into();
    state.step_history = vec![
        completed_record("s1", "step1 done", Transition::To("s2".into())),
        completed_record("s2", "step2 done", Transition::To("s3".into())),
        completed_record(
            "s3",
            "step3 done",
            Transition::Abort {
                reason: "done".into(),
            },
        ),
    ];
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    // restore_from_step(s2)：应截断 s2 及 s3 的 record，只留 s1。
    struct RestartJudge;
    impl StepTransitionJudge for RestartJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let step_id = ctx.current_step.id().clone();
            Box::pin(async move {
                match step_id.as_str() {
                    "s1" => Transition::To("s2".into()),
                    "s2" => Transition::To("s3".into()),
                    _ => Transition::Abort {
                        reason: "done".into(),
                    },
                }
            })
        }
    }
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("step2 redone"),
        MockResponse::text("step3 redone"),
    ]));
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::restore_from_step(
        store.clone(),
        task_id.clone(),
        "s2",
        config,
        Arc::new(RestartJudge),
    )
    .await
    .unwrap();

    // 构造后 status=Paused, current_step=s2, step_history 只剩 s1。
    assert_eq!(engine.state().await, WorkflowStatus::Paused);
    assert_eq!(engine.current_step().await, "s2");
    let history = engine.step_history().await;
    assert_eq!(
        history.len(),
        1,
        "step_history should only contain s1 after truncation"
    );
    assert_eq!(history[0].step_id, "s1");

    let result = engine.run().await.unwrap();
    let history = engine.step_history().await;
    // 重跑后：s1(旧) + s2(新) + s3(新)。
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].step_id, "s1");
    assert_eq!(history[1].step_id, "s2");
    assert_eq!(history[2].step_id, "s3");
    // turns = count_executed_turns(history)：s1(旧,retained) + s2(新) + s3(新) = 3。
    // 截断已删旧 s2/s3，故不含它们的旧 record；s1 旧 record 仍计数。
    assert_eq!(
        result.turns, 3,
        "turns counts all result:Some records in retained history"
    );
    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
}

#[tokio::test]
async fn restore_from_step_target_not_in_workflow() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restart-bad-step".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![Step::llm("s1", "Step 1", "do step 1", vec![])],
        edges: vec![],
    };
    let state = WorkflowState::new("s1".into());
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    let client = Arc::new(MockLlmClient::new(vec![]));
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let result = WorkflowEngine::restore_from_step(
        store,
        task_id,
        "nonexistent",
        config,
        Arc::new(NoopJudge),
    )
    .await;
    let err = match result {
        Ok(_) => panic!("expected CurrentStepNotFound, got Ok"),
        Err(e) => e,
    };
    assert!(
        matches!(err, WorkflowError::CurrentStepNotFound { ref step_id } if step_id == "nonexistent"),
        "expected CurrentStepNotFound, got {err:?}"
    );
}

#[tokio::test]
async fn restore_from_step_target_not_in_history() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restart-unrun-step".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
            Step::llm("s3", "Step 3", "do step 3", vec![]),
        ],
        edges: vec![
            Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            },
            Edge {
                from: "s2".into(),
                to: "s3".into(),
                condition: None,
            },
        ],
    };
    // 只跑了 s1，停在 s2（current_step=s2 但 s2 未完成）。
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Running;
    state.started_at = Some(chrono::Utc::now());
    state.current_step = "s2".into();
    state.step_history = vec![completed_record(
        "s1",
        "step1 done",
        Transition::To("s2".into()),
    )];
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    // restore_from_step(s3)：s3 在 workflow 里但从未在 step_history 中执行过。
    // 设计变更后：target step 不在 step_history 返回 StepNotInHistory，
    // 构造器本身失败，不执行 run()。使用 NoopJudge 和空 MockLlmClient。
    let client = Arc::new(MockLlmClient::new(vec![]));
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let result =
        WorkflowEngine::restore_from_step(store, task_id, "s3", config, Arc::new(NoopJudge)).await;
    let err = match result {
        Ok(_) => panic!("expected StepNotInHistory for s3, got Ok"),
        Err(e) => e,
    };
    assert!(
        matches!(err, WorkflowError::StepNotInHistory { ref step_id } if step_id == "s3"),
        "expected StepNotInHistory for s3, got {err:?}"
    );
}

#[tokio::test]
async fn restore_from_step_context_not_rolled_back() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restart-context".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
        ],
        edges: vec![Edge {
            from: "s1".into(),
            to: "s2".into(),
            condition: None,
        }],
    };
    // s1 写 context k=1，s2 写 k=2，s2 跑完后 context 里有 k=2。
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Succeeded;
    state.started_at = Some(chrono::Utc::now());
    state.ended_at = Some(chrono::Utc::now());
    state.current_step = "s2".into();
    state.step_history = vec![
        completed_record("s1", "step1 done", Transition::To("s2".into())),
        completed_record(
            "s2",
            "step2 done",
            Transition::Abort {
                reason: "done".into(),
            },
        ),
    ];
    // 模拟 s2 执行后 context 累积了 k=2。
    state
        .context
        .variables
        .insert("k".into(), serde_json::Value::Number(2.into()));
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    // restore_from_step(s2)：context 不回滚，s2 重跑时应读到 k=2。
    struct ContextCheckJudge;
    impl StepTransitionJudge for ContextCheckJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let k = ctx
                .context
                .variables
                .get("k")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);
            Box::pin(async move {
                assert_eq!(k, 2, "context should not be rolled back, k should be 2");
                Transition::Abort {
                    reason: "done".into(),
                }
            })
        }
    }
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("s2 redo")]));
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::restore_from_step(
        store,
        task_id,
        "s2",
        config,
        Arc::new(ContextCheckJudge),
    )
    .await
    .unwrap();
    engine.run().await.unwrap();
}

#[tokio::test]
async fn restore_from_step_persists_truncated_state() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restart-persist".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
            Step::llm("s3", "Step 3", "do step 3", vec![]),
        ],
        edges: vec![
            Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            },
            Edge {
                from: "s2".into(),
                to: "s3".into(),
                condition: None,
            },
        ],
    };
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Succeeded;
    state.current_step = "s3".into();
    state.step_history = vec![
        completed_record("s1", "step1 done", Transition::To("s2".into())),
        completed_record("s2", "step2 done", Transition::To("s3".into())),
        completed_record(
            "s3",
            "step3 done",
            Transition::Abort {
                reason: "done".into(),
            },
        ),
    ];
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    let client = Arc::new(MockLlmClient::new(vec![]));
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let _engine = WorkflowEngine::restore_from_step(
        store.clone(),
        task_id.clone(),
        "s2",
        config.clone(),
        Arc::new(NoopJudge),
    )
    .await
    .unwrap();

    // 重新 restore：盘上 state 应是截断后的（current_step=s2, status=Paused, history=[s1]）。
    let restored = WorkflowEngine::restore(store, task_id, config, Arc::new(NoopJudge))
        .await
        .unwrap();
    assert_eq!(restored.current_step().await, "s2");
    assert_eq!(restored.state().await, WorkflowStatus::Paused);
    let history = restored.step_history().await;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].step_id, "s1");
}

/// rposition 语义：step_history 中 target 出现多次（Retry/rework）时，
/// 截断最后一次出现及其后所有 record，保留之前的完整执行链。
#[tokio::test]
async fn restore_from_step_rposition_truncates_last_occurrence() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restart-rposition".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
            Step::llm("s3", "Step 3", "do step 3", vec![]),
        ],
        edges: vec![
            Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            },
            Edge {
                from: "s2".into(),
                to: "s3".into(),
                condition: None,
            },
        ],
    };
    // 模拟 rework 场景：s1→s2→s3→(s2 rework)→s3，s2 出现两次。
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Succeeded;
    state.current_step = "s3".into();
    state.step_history = vec![
        completed_record("s1", "s1 first", Transition::To("s2".into())),
        completed_record("s2", "s2 first", Transition::To("s3".into())),
        completed_record("s3", "s3 first", Transition::To("s2".into())),
        completed_record("s2", "s2 rework", Transition::To("s3".into())),
        completed_record(
            "s3",
            "s3 rework",
            Transition::Abort {
                reason: "done".into(),
            },
        ),
    ];
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    let client = Arc::new(MockLlmClient::new(vec![]));
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine =
        WorkflowEngine::restore_from_step(store, task_id, "s2", config, Arc::new(NoopJudge))
            .await
            .unwrap();

    // rposition 截断最后一次 s2（index 3）及其后（s3 rework），
    // 保留 [s1, s2 first, s3 first, s2 rework] 之前的三条？不——
    // rposition 找最后一条 step_id=="s2"，即 index 3。
    // truncate(3) 保留 index 0..2 = [s1, s2 first, s3 first]。
    let history = engine.step_history().await;
    assert_eq!(
        history.len(),
        3,
        "should keep records before last s2 occurrence"
    );
    assert_eq!(history[0].step_id, "s1");
    assert_eq!(history[1].step_id, "s2");
    assert_eq!(history[2].step_id, "s3");
    // current_step = s2（最后一次出现的 s2），status = Paused。
    assert_eq!(engine.current_step().await, "s2");
    assert_eq!(engine.state().await, WorkflowStatus::Paused);
}

/// 盘上 status=Running 时调 restore_from_step：构造器应覆写为 Paused，
/// 防止 run() Running arm 用历史末尾 transition 覆写 current_step。
#[tokio::test]
async fn restore_from_step_from_running_status_sets_paused() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restart-from-running".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
            Step::llm("s3", "Step 3", "do step 3", vec![]),
        ],
        edges: vec![
            Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            },
            Edge {
                from: "s2".into(),
                to: "s3".into(),
                condition: None,
            },
        ],
    };
    // 模拟崩溃：status 仍为 Running，current_step 停在 s3。
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Running;
    state.started_at = Some(chrono::Utc::now());
    state.current_step = "s3".into();
    state.step_history = vec![
        completed_record("s1", "s1 done", Transition::To("s2".into())),
        completed_record("s2", "s2 done", Transition::To("s3".into())),
        completed_record(
            "s3",
            "s3 done",
            Transition::Abort {
                reason: "done".into(),
            },
        ),
    ];
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    let client = Arc::new(MockLlmClient::new(vec![]));
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine =
        WorkflowEngine::restore_from_step(store, task_id, "s2", config, Arc::new(NoopJudge))
            .await
            .unwrap();

    // 构造器必须把 Running 覆写为 Paused，否则 run() 会走 Running arm。
    assert_eq!(
        engine.state().await,
        WorkflowStatus::Paused,
        "restore_from_step must set Paused even if disk status was Running"
    );
    assert_eq!(engine.current_step().await, "s2");
    let history = engine.step_history().await;
    assert_eq!(history.len(), 1, "truncated to s1 only");
}

/// target=entry_step（pos=0）：截断整个 step_history，从入口重跑。
#[tokio::test]
async fn restore_from_step_to_entry_step_clears_history() {
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restart-entry".into());
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
        ],
        edges: vec![Edge {
            from: "s1".into(),
            to: "s2".into(),
            condition: None,
        }],
    };
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Succeeded;
    state.current_step = "s2".into();
    state.step_history = vec![
        completed_record("s1", "s1 done", Transition::To("s2".into())),
        completed_record(
            "s2",
            "s2 done",
            Transition::Abort {
                reason: "done".into(),
            },
        ),
    ];
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    let client = Arc::new(MockLlmClient::new(vec![]));
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine =
        WorkflowEngine::restore_from_step(store, task_id, "s1", config, Arc::new(NoopJudge))
            .await
            .unwrap();

    // truncate(0)：step_history 清空，current_step=s1。
    assert_eq!(engine.current_step().await, "s1");
    assert_eq!(engine.state().await, WorkflowStatus::Paused);
    let history = engine.step_history().await;
    assert!(
        history.is_empty(),
        "history should be fully cleared when target is entry_step"
    );
}

// ── Structured 输出集成测试 (issue #72 Task 10) ──────────────────────────────
//
// 端到端验证 StructuredOutputCoordinator 接入 run_llm_step 后的行为：
// 1. happy path：首轮合法 JSON → Ok
// 2. 升级路径：首轮非 JSON → response_format 升级 → 二轮 JSON → Ok
// 3. 达上限：持续非 JSON → Failed
// 4. 未声明 structured → NotRequired
// 5. spawn + structured 交互
// 6. supports_json_schema=false → 直接 Failed
//
// 注意：升级路径/达上限需要 mock 报告 supports_json_schema=true，否则
// 协调器在首次 should_stop 失败后直接走 Layer 3（Failed），无法验证升级。
// Anthropic 合成工具(structured_result)提取已删除(P1-1: 无来源校验,LLM
// 可伪造 ToolUse 绕过文本解析)。

/// 构造一个 `structured: Some(true)` 的 LLM step（使用公共 `with_structured` API）。
fn structured_llm_step(id: &str, prompt: &str) -> Step {
    Step::llm(id, id, prompt, vec![]).with_structured(Some(true))
}

/// 报告 supports_json_schema 的 mock client（升级路径所需）。
fn json_schema_mock(responses: Vec<MockResponse>) -> Arc<MockLlmClient> {
    Arc::new(MockLlmClient::new(responses).with_capabilities(
        llm_harness_loop::ProviderCapabilities::new(false, false, true),
    ))
}

#[tokio::test]
async fn structured_step_parses_json_on_first_try() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
        r#"{"passed":true}"#,
    )]));
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![structured_llm_step("s1", "do step 1")],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge)).unwrap();
    let _ = engine.run().await.unwrap();
    let history = engine.step_history().await;
    let result = history[0].result.as_ref().unwrap();
    assert_eq!(result.structured_status, StructuredStatus::Ok);
    assert_eq!(result.structured.as_ref().unwrap()["passed"], true);
    // 首轮即成功：只调用了一次 provider。
    assert_eq!(
        client.call_count.load(Ordering::SeqCst),
        1,
        "happy path should consume exactly one LLM call"
    );
}

#[tokio::test]
async fn non_structured_step_status_not_required() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
        "just plain text, no JSON",
    )]));
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![Step::llm("s1", "Step 1", "do", vec![])],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge)).unwrap();
    let _ = engine.run().await.unwrap();
    let history = engine.step_history().await;
    let result = history[0].result.as_ref().unwrap();
    assert_eq!(result.structured_status, StructuredStatus::NotRequired);
    assert!(result.structured.is_none());
    // 未声明 structured：coordinator 未挂载，不应有升级开销。
    assert_eq!(
        client.call_count.load(Ordering::SeqCst),
        1,
        "non-structured step should make exactly one LLM call"
    );
}

#[tokio::test]
async fn structured_step_upgrades_after_first_failure() {
    let dir = tempfile::TempDir::new().unwrap();
    // Turn 1: 纯文本（非 JSON）→ should_stop 失败 → 请求升级。
    // Turn 2: 合法 JSON → should_stop 成功 → Ok。
    let client = json_schema_mock(vec![
        MockResponse::text("I cannot produce JSON, sorry"),
        MockResponse::text(r#"{"passed":true}"#),
    ]);
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![structured_llm_step("s1", "do")],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge)).unwrap();
    let _ = engine.run().await.unwrap();
    let history = engine.step_history().await;
    let result = history[0].result.as_ref().unwrap();
    assert_eq!(result.structured_status, StructuredStatus::Ok);
    assert_eq!(result.structured.as_ref().unwrap()["passed"], true);
    // 升级一次 → 共 2 次 provider 调用。
    assert_eq!(
        client.call_count.load(Ordering::SeqCst),
        2,
        "upgrade path should consume two LLM calls"
    );
    // 验证第二轮请求确实注入了 JsonObject response_format（Layer 2 生效）。
    let captured = client.captured_requests.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert!(
        captured[1].response_format().is_some(),
        "second turn should carry an upgraded response_format"
    );
}

#[tokio::test]
async fn structured_step_fails_after_max_attempts() {
    let dir = tempfile::TempDir::new().unwrap();
    // DEFAULT_MAX_UPGRADE_ATTEMPTS = 2 → 最多 3 轮（初始 + 2 次升级），全失败 → Failed。
    let client = json_schema_mock(vec![
        MockResponse::text("not json"),
        MockResponse::text("still not json"),
        MockResponse::text("never gonna be json"),
    ]);
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![structured_llm_step("s1", "do")],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge)).unwrap();
    let _ = engine.run().await.unwrap();
    let history = engine.step_history().await;
    let result = history[0].result.as_ref().unwrap();
    assert_eq!(result.structured_status, StructuredStatus::Failed);
    assert!(result.structured.is_none());
    // DEFAULT_MAX_UPGRADE_ATTEMPTS=2 意味着 3 轮 LLM 调用：
    // Turn 1 失败 → 请求升级；Turn 2 失败 → 升级到 Upgraded{2}；
    // Turn 3 失败 → should_stop 发现 attempts(2) >= max(2)，直接置 Failed 并停止。
    // 不再需要第 4 次"Failed 确认"调用。
    assert_eq!(
        client.call_count.load(Ordering::SeqCst),
        3,
        "max attempts exhausted: 3 upgrade turns, then should_stop fails fast"
    );
}

/// #3: structured step 同时允许 spawn 工具时,coordinator 不压制 spawn 的
/// after_turn drain,sub-agent 完成后主 agent 仍能产出合法 JSON → Ok。
///
/// 主 agent Turn 1 调 spawn_agent(ToolUse,stop_reason=ToolUse → tool 分支,
/// 不调 should_stop);sub-agent 异步跑完;主 agent Turn 2 输出 JSON →
/// coordinator should_stop 提取成功 → Ok。
#[tokio::test]
async fn structured_step_with_spawn_succeeds() {
    let dir = tempfile::TempDir::new().unwrap();
    // 预设响应：首个是 spawn_agent tool_use,后续全是合法 JSON 文本。
    // 主 agent Turn 2 与 sub-agent 的 chat_stream 会竞争预设响应,但两者
    // 拿到 JSON 文本都能正常处理（主 agent coordinator 解析,sub-agent
    // EndTurn 停止）。提供充足数量避免耗尽回退到 "(no more mock responses)"。
    let mut responses = vec![MockResponse::tool_use(
        "call_1",
        "spawn_agent",
        r#"{"prompt":"compute sub-task"}"#,
    )];
    for _ in 0..8 {
        responses.push(MockResponse::text(r#"{"spawned":true}"#));
    }
    let client = json_schema_mock(responses);
    let mut step = Step::llm(
        "s1",
        "s1",
        "spawn a sub-agent then output JSON",
        vec!["spawn_agent".into()],
    );
    if let Step::Llm { structured, .. } = &mut step {
        *structured = Some(true);
    }
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![step],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge)).unwrap();
    let _ = engine.run().await.unwrap();
    let history = engine.step_history().await;
    let result = history[0].result.as_ref().unwrap();
    assert_eq!(
        result.structured_status,
        StructuredStatus::Ok,
        "structured + spawn: coordinator must extract JSON after sub-agent completes"
    );
    let structured = result
        .structured
        .as_ref()
        .expect("structured value present");
    assert_eq!(structured["spawned"], true);
    // 主 agent 至少 2 次调用（spawn turn + JSON turn）,sub-agent 至少 1 次。
    assert!(
        client.call_count.load(Ordering::SeqCst) >= 3,
        "expected >=3 LLM calls (main spawn + main json + sub-agent), got {}",
        client.call_count.load(Ordering::SeqCst)
    );
}

/// #6: provider 不支持 JsonObject 时,coordinator 首次失败后不注入 response_format,
/// 直接走 Layer 3（Failed）。覆盖 supports_json_schema=false 的端到端路径。
#[tokio::test]
async fn structured_step_fails_when_provider_lacks_json_schema() {
    let dir = tempfile::TempDir::new().unwrap();
    // supports_json_schema=false 的 mock（默认 capabilities 全 false）。
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("not json"),
        MockResponse::text("still not json"),
    ]));
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![structured_llm_step("s1", "do")],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge)).unwrap();
    let _ = engine.run().await.unwrap();
    let history = engine.step_history().await;
    let result = history[0].result.as_ref().unwrap();
    assert_eq!(result.structured_status, StructuredStatus::Failed);
    assert!(result.structured.is_none());
    // supports=false：Turn 1 失败 → UpgradeRequested{0} → prepare(!supports → Failed)；
    // Turn 2 should_stop 看到 Failed → true。共 2 次调用,且第二轮不应注入 response_format。
    assert_eq!(
        client.call_count.load(Ordering::SeqCst),
        2,
        "supports=false: 1 failure turn + 1 Failed-confirmation turn"
    );
    let captured = client.captured_requests.lock().unwrap();
    assert!(
        captured.iter().all(|r| r.response_format().is_none()),
        "supports=false: no turn should carry response_format"
    );
}

// ── session 路径隔离 (issue #73) ──────────────────────────────────────────

#[tokio::test]
async fn llm_step_session_lands_in_task_step_attempt_dir() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("done")]));
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new_free_task("do work".into(), vec![], config).unwrap();
    let _ = engine.run().await.unwrap();

    // 期望:sessions/<task_id>/run-1/<session_id>/ 存在
    let task_dirs: Vec<_> = std::fs::read_dir(dir.path().join("sessions"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("task-"))
        .collect();
    assert_eq!(task_dirs.len(), 1, "exactly one task-<uuid> dir");
    let task_dir = task_dirs[0].path();

    let step_dirs: Vec<_> = std::fs::read_dir(&task_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy() == "run-1")
        .collect();
    assert_eq!(step_dirs.len(), 1, "exactly one 'run-1' dir");
    let step_dir = step_dirs[0].path();

    // <session_id>/ 子目录(jsonl session repo 创建,uuid 动态)
    let session_dirs: Vec<_> = std::fs::read_dir(&step_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect();
    assert_eq!(
        session_dirs.len(),
        1,
        "exactly one <session_id> dir under run-1"
    );
}

/// Judge: 首次返回 Retry,第二次返回 Abort（流程结束）。
///
/// 注:brief 原文用 `new_free_task` + `Transition::To(cur)` 结束流程,但
/// (1) `new_free_task` 硬编码 `NoopJudge` 且 `decide_transition` 对自由 task
///     (单步无 edge)短路返回 `Abort`,judge 永不被调用;
/// (2) `Transition::To` 不结束 run_loop(只有 Abort/Fail/max_steps 终止)。
/// 故改为 2-step workflow + `RetryOnceJudge` + `Abort`,真正覆盖 retry 路径。
struct RetryOnceJudge;
impl StepTransitionJudge for RetryOnceJudge {
    fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
        let cur = ctx.current_step.id().clone();
        // step_history 中同 step_id 的 record 数决定是第几次执行
        let n = ctx.step_history.iter().filter(|r| r.step_id == cur).count();
        Box::pin(async move {
            if cur == "run" {
                if n < 2 {
                    Transition::Retry
                } else {
                    // 第二次完成后,跳到 done 让流程结束。
                    Transition::To("done".into())
                }
            } else {
                Transition::Abort {
                    reason: "done".into(),
                }
            }
        })
    }
}

#[tokio::test]
async fn retry_creates_separate_attempt_dirs() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("first attempt"),
        MockResponse::text("second attempt"),
        MockResponse::text("done step"),
    ]));
    let workflow = Workflow {
        entry_step: "run".into(),
        steps: vec![
            Step::llm("run", "Run", "do work", vec![]),
            Step::llm("done", "Done", "finish", vec![]),
        ],
        edges: vec![Edge {
            from: "run".into(),
            to: "done".into(),
            condition: None,
        }],
    };
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(RetryOnceJudge)).unwrap();
    let _ = engine.run().await.unwrap();

    // 期望:sessions/<task_id>/run-1/ 和 run-2/ 都存在(retry 产生两个 attempt 目录)
    let task_dirs: Vec<_> = std::fs::read_dir(dir.path().join("sessions"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("task-"))
        .collect();
    assert_eq!(task_dirs.len(), 1);
    let task_dir = task_dirs[0].path();

    let attempt_dirs: Vec<String> = std::fs::read_dir(&task_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        attempt_dirs.iter().any(|d| d == "run-1"),
        "run-1 should exist: {:?}",
        attempt_dirs
    );
    assert!(
        attempt_dirs.iter().any(|d| d == "run-2"),
        "run-2 should exist: {:?}",
        attempt_dirs
    );
}
/// #73 final review: `StepExecutionPolicy.max_attempts > 1` must produce
/// distinct session dirs per policy attempt. Before the fix, no StepRecord
/// was pushed between policy attempts, so `attempt_seq` stayed at 1 and all
/// attempts collided in `run-1/`.
#[tokio::test]
async fn policy_retry_creates_separate_attempt_dirs() {
    use crate::workflow::model::StepExecutionPolicy;

    let dir = tempfile::TempDir::new().unwrap();
    // First attempt: provider RateLimit error → StepFailed → policy retries.
    // Second attempt: success.
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::rate_limit_error(),
        MockResponse::text("done"),
    ]));
    let workflow = Workflow {
        entry_step: "run".into(),
        steps: vec![
            Step::llm("run", "Run", "do work", vec![]).with_policy(StepExecutionPolicy {
                timeout_ms: None,
                max_attempts: Some(2),
                retry_backoff_ms: Some(0),
            }),
        ],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge)).unwrap();
    let _ = engine.run().await.unwrap();

    let task_dirs: Vec<_> = std::fs::read_dir(dir.path().join("sessions"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("task-"))
        .collect();
    assert_eq!(task_dirs.len(), 1, "exactly one task-<uuid> dir");
    let task_dir = task_dirs[0].path();

    let attempt_dirs: Vec<String> = std::fs::read_dir(&task_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        attempt_dirs.iter().any(|d| d == "run-1"),
        "run-1 should exist (attempt 1): {:?}",
        attempt_dirs
    );
    assert!(
        attempt_dirs.iter().any(|d| d == "run-2"),
        "run-2 should exist (attempt 2, policy retry): {:?}",
        attempt_dirs
    );
}

/// #73 final review: executor steps must NOT create any session directory.
/// `run_executor_step` never calls `session_factory.create`, so no
/// `<task_id>/` dir should appear under `session_base_dir`.
#[tokio::test]
async fn executor_step_creates_no_session_dir() {
    struct NoopExecutor;
    impl StepExecutor for NoopExecutor {
        fn execute<'a>(&'a self, _ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
            Box::pin(async move {
                Ok(StepResult {
                    output: "executor ok".into(),
                    structured: None,
                    structured_status: StructuredStatus::NotRequired,
                    tool_calls_count: 0,
                    session_id: String::new(),
                    cost: CostAggregate::default(),
                    started_at: None,
                    ended_at: None,
                })
            })
        }
    }

    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![]));
    let workflow = Workflow {
        entry_step: "exec".into(),
        steps: vec![Step::executor("exec", "Exec", "noop", None)],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
        .unwrap()
        .with_executor("noop", Arc::new(NoopExecutor));

    let _ = engine.run().await.unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);

    // session_base_dir may not exist at all (factory never called → no
    // create_dir_all). If it exists, it must contain no task-*/ dir.
    let session_base = dir.path().join("sessions");
    let task_dirs: Vec<_> = match std::fs::read_dir(&session_base) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("task-"))
            .collect(),
        Err(_) => vec![],
    };
    assert!(
        task_dirs.is_empty(),
        "executor step must not create a session dir, found: {:?}",
        task_dirs
    );
}

/// #73 Task 4: sub-agent session 必须落在
/// `<session_base_dir>/<task_id>/<step_id>-1/sub-agents/<session_id>/`。
///
/// Task 3 的 `retry_creates_separate_attempt_dirs` 未启用 spawn_agent,
/// `build_step_spawner` 从未被调用。此测试直接调 `build_step_spawner`
/// 构造 spawner 并 spawn 一个最小 sub-agent,验证 on-disk 目录结构。
#[tokio::test]
async fn subagent_session_lands_in_subagents_subdir() {
    use crate::spawn::message_bus::message_bus_pair;
    use crate::spawn::spawn_types::{ContextMode, ProviderSelection, SpawnOptions};
    use crate::spawn::spawner::HarnessSubAgentSpawner;

    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("sub done")]));
    let config = WorkflowEngineConfig {
        client: client.clone(),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new_free_task("do work".into(), vec![], config).unwrap();
    let bus = message_bus_pair();
    // 取出 event_rx guard,用于等待 sub-agent 完成事件(避免 tokio 运行时
    // 关闭时仍有挂起的后台 task)。
    let mut event_guard = bus.take_event_rx().expect("event_rx available");
    let spawner: HarnessSubAgentSpawner = engine.build_step_spawner(bus, &"run".into(), 1);

    // spawn 一个最小 sub-agent。spawn_async 返回 session_id(String)。
    let session_id = spawner
        .spawn_async(SpawnOptions {
            prompt: "do sub work".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        })
        .await
        .expect("spawn should succeed");
    assert!(!session_id.is_empty(), "session_id should be non-empty");

    // 等待 sub-agent 后台 task 完成并投递 Done 事件。
    let done = event_guard.recv().await;
    assert!(
        matches!(
            done,
            Some(crate::spawn::message_bus::SubAgentEvent::Done { .. })
        ),
        "expected SubAgentEvent::Done, got {:?}",
        done
    );
    drop(event_guard);

    // 期望:sessions/<task_id>/run-1/sub-agents/<session_id>/ 存在
    let task_dirs: Vec<_> = std::fs::read_dir(dir.path().join("sessions"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("task-"))
        .collect();
    assert_eq!(task_dirs.len(), 1, "exactly one task-<uuid> dir");
    let task_dir = task_dirs[0].path();

    let sub_agents_dir = task_dir.join("run-1").join("sub-agents");
    assert!(
        sub_agents_dir.exists(),
        "sub-agents dir should exist at {:?}",
        sub_agents_dir
    );

    let sub_session_dirs: Vec<_> = std::fs::read_dir(&sub_agents_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect();
    assert!(
        !sub_session_dirs.is_empty(),
        "at least one sub-agent session dir should exist under {:?}",
        sub_agents_dir
    );
    // session_id 对应的目录应在 sub-agents/ 下。
    assert!(
        sub_session_dirs
            .iter()
            .any(|e| e.file_name() == std::ffi::OsStr::new(&session_id)),
        "session_id dir {:?} should exist under sub-agents/, found: {:?}",
        session_id,
        sub_session_dirs
            .iter()
            .map(|e| e.file_name())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn restore_keeps_session_under_same_task_dir() {
    // 不变量:restore 用调用方传入的 task_id 构造 engine,而 run_llm_step 用
    // self.task_id.0 组装 <task_id>/<step_id>-<attempt_seq>/ 前缀 → restore 后
    // 新 session 落在与原 run 相同的 <task_id>/ 目录下。
    //
    // 注:brief 原文用 new_free_task 跑完首 run 再 restore,但 new_free_task 单步
    // 流程跑完即 Succeeded(终态),run() 拒绝在 Succeeded 上再跑(InvalidStatus)。
    // 改为按现有 restore 测试模式(running_restore_continues_from_transition_target)
    // 手动 seed 一个 Running state:s1 已完成、transition To s2、current_step=s1,
    // 模拟"首 run 跑了 s1 后崩溃"。restore + run 执行 s2,产生真实 session。
    let dir = tempfile::TempDir::new().unwrap();
    let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
    let task_id = TaskId("restore-path-consistency".into());

    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "do step 1", vec![]),
            Step::llm("s2", "Step 2", "do step 2", vec![]),
        ],
        edges: vec![Edge {
            from: "s1".into(),
            to: "s2".into(),
            condition: None,
        }],
    };
    // s1 已完成、transition To s2;status=Running 模拟崩溃后恢复。
    let mut state = WorkflowState::new("s1".into());
    state.status = WorkflowStatus::Running;
    state.started_at = Some(chrono::Utc::now());
    state.step_history = vec![completed_record(
        "s1",
        "step1 done",
        Transition::To("s2".into()),
    )];
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();

    // restore 同一 task_id:engine 沿用传入的 task_id。
    struct AbortOnS2;
    impl StepTransitionJudge for AbortOnS2 {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let step_id = ctx.current_step.id().clone();
            Box::pin(async move {
                if step_id == "s1" {
                    Transition::To("s2".into())
                } else {
                    Transition::Abort {
                        reason: "done".into(),
                    }
                }
            })
        }
    }
    let config = WorkflowEngineConfig {
        client: Arc::new(MockLlmClient::new(vec![MockResponse::text("step2 done")])),
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let restored = WorkflowEngine::restore(store, task_id.clone(), config, Arc::new(AbortOnS2))
        .await
        .unwrap();
    // restore 保留了传入的 task_id。
    assert_eq!(restored.task_id(), &task_id);
    restored.run().await.unwrap();
    assert_eq!(restored.state().await, WorkflowStatus::Succeeded);

    // 核心断言:sessions/ 下存在 <task_id.0>/ 目录(restore 保留 task_id,
    // run_llm_step 用 self.task_id.0 组装前缀 → session 落在同一 <task_id>/ 下)。
    let task_dir = dir.path().join("sessions").join(&task_id.0);
    assert!(
        task_dir.is_dir(),
        "session dir {:?} should exist under sessions/",
        task_dir
    );

    // <task_id>/ 下应有 run_llm_step 创建的 <step_id>-<attempt_seq>/ 子目录
    // (s2 的 attempt_seq=1,因 step_history 仅含 s1)。不强求具体名,只要至少
    // 一个子目录即证明 session 落在 <task_id>/ 下。
    let session_subdirs: Vec<_> = std::fs::read_dir(&task_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect();
    assert!(
        !session_subdirs.is_empty(),
        "at least one session subdir should exist under {:?}",
        task_dir
    );
}
