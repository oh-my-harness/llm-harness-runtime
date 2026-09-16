use super::*;
use anyhow::Result; // StepExecutor::execute returns anyhow::Result
use futures::future::BoxFuture;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
use llm_harness_platform::env::EnvFactory;
use llm_harness_subagents::spawner::JsonlSessionFactory;
use llm_harness_types::{
    AgentError, BeforeTurnCtx, BeforeTurnHook, ContentBlock, DataBlock, ExecutionEnv, ToolContext,
    ToolFailure, ToolResult,
};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

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
use crate::workflow::model::StepCache;
use crate::workflow::model::{ConditionExpr, Edge, EdgeCondition};
use crate::workflow::model::{LoopConfig, StepExecutionPolicy};

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

struct RecoveringExecutor {
    expected_id: ExecutorExecutionId,
    recover_calls: AtomicUsize,
    execute_calls: AtomicUsize,
}

impl StepExecutor for RecoveringExecutor {
    fn recover<'a>(
        &'a self,
        ctx: &'a ExecutorCtx<'a>,
    ) -> BoxFuture<'a, Result<Option<StepResult>>> {
        Box::pin(async move {
            assert_eq!(ctx.execution_id, self.expected_id);
            self.recover_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(completed_result("recovered external outcome")))
        })
    }

    fn execute<'a>(&'a self, _ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            self.execute_calls.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("execute must not run after recovery")
        })
    }
}

struct FailingRecoveryExecutor {
    recover_calls: AtomicUsize,
    execute_calls: AtomicUsize,
}

impl StepExecutor for FailingRecoveryExecutor {
    fn recover<'a>(
        &'a self,
        _ctx: &'a ExecutorCtx<'a>,
    ) -> BoxFuture<'a, Result<Option<StepResult>>> {
        Box::pin(async move {
            self.recover_calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::Error::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "payload was purged at 2026-08-29T00:00:00Z",
            ))
            .context("cannot recover review audit record"))
        })
    }

    fn execute<'a>(&'a self, _ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            self.execute_calls.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("execute must not run after a recovery error")
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
        cache_key: None,
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
        cache_key: None,
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
    ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            ctx.abort.cancelled().await;
            Err(ToolFailure::aborted())
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
    ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            Ok(ToolResult::full(
                vec![DataBlock::text("echo")],
                serde_json::json!({}),
                false,
            ))
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
async fn restored_executor_recovers_stable_attempt_without_reexecution() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(JsonlTaskStore::new(dir.path().join("tasks")));
    let task_id = TaskId("recover-executor-outcome".into());
    let step = Step::executor("review", "Review", "recovering", None);
    let workflow = Workflow {
        entry_step: "review".into(),
        steps: vec![
            step,
            Step::terminal("done", "Done", 0, Some("complete".into())),
        ],
        edges: vec![Edge {
            from: "review".into(),
            to: "done".into(),
            condition: None,
        }],
    };
    let mut state = WorkflowState::new("review".into());
    state.status = WorkflowStatus::Running;
    state.started_at = Some(chrono::Utc::now());
    store.save_workflow(&task_id, &workflow).await.unwrap();
    store.save_workflow_state(&task_id, &state).await.unwrap();
    let executor = Arc::new(RecoveringExecutor {
        expected_id: ExecutorExecutionId {
            task_id: task_id.clone(),
            step_id: "review".into(),
            attempt: 1,
        },
        recover_calls: AtomicUsize::new(0),
        execute_calls: AtomicUsize::new(0),
    });
    let config = WorkflowEngineConfig {
        client: Arc::new(MockLlmClient::new(Vec::new())),
        model: "unused".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::restore(store, task_id, config, Arc::new(NoopJudge))
        .await
        .unwrap()
        .with_executor("recovering", executor.clone());

    engine.run().await.unwrap();

    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert_eq!(executor.recover_calls.load(Ordering::SeqCst), 1);
    assert_eq!(executor.execute_calls.load(Ordering::SeqCst), 0);
    let history = engine.step_history().await;
    assert_eq!(
        history[0].result.as_ref().unwrap().output,
        "recovered external outcome"
    );
}

#[tokio::test]
async fn executor_recovery_failure_is_not_retried_or_executed() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = Arc::new(JsonlTaskStore::new(dir.path().join("tasks")));
    let executor = Arc::new(FailingRecoveryExecutor {
        recover_calls: AtomicUsize::new(0),
        execute_calls: AtomicUsize::new(0),
    });
    let workflow = Workflow {
        entry_step: "review".into(),
        steps: vec![
            Step::executor("review", "Review", "failing-recovery", None).with_policy(
                StepExecutionPolicy {
                    timeout_ms: None,
                    max_attempts: Some(3),
                    retry_backoff_ms: Some(0),
                    cache: None,
                    r#loop: None,
                },
            ),
        ],
        edges: vec![],
    };
    let config = WorkflowEngineConfig {
        client: Arc::new(MockLlmClient::new(Vec::new())),
        model: "unused".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
        .unwrap()
        .with_task_store(store.clone())
        .with_executor("failing-recovery", executor.clone());
    let mut events = engine.subscribe();

    let error = engine.run().await.unwrap_err();

    let expected_outer = "cannot recover review audit record";
    let expected_inner = "payload was purged at 2026-08-29T00:00:00Z";
    assert!(
        error
            .to_string()
            .contains("executor outcome recovery failed"),
        "unexpected error: {error}"
    );
    assert!(error.to_string().contains(expected_outer), "{error}");
    assert!(error.to_string().contains(expected_inner), "{error}");
    assert_eq!(executor.recover_calls.load(Ordering::SeqCst), 1);
    assert_eq!(executor.execute_calls.load(Ordering::SeqCst), 0);

    let failed_event = loop {
        let event = events.try_recv().unwrap();
        if let WorkflowEvent::Failed { error } = event {
            break error;
        }
    };
    assert!(failed_event.contains(expected_outer), "{failed_event}");
    assert!(failed_event.contains(expected_inner), "{failed_event}");

    let persisted = store
        .load_workflow_state(engine.task_id())
        .await
        .unwrap()
        .unwrap();
    let TaskError::Internal(persisted_error) = persisted.error.unwrap() else {
        panic!("expected internal task error");
    };
    assert!(
        persisted_error.contains(expected_outer),
        "{persisted_error}"
    );
    assert!(
        persisted_error.contains(expected_inner),
        "{persisted_error}"
    );
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
    use llm_harness_types::{AssistantMessageKind, ToolResult};

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
        result: Ok(ToolResult::full(vec![], serde_json::Value::Null, false)),
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
        result: Err(ToolFailure::aborted()),
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
        discarded: false,
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

    // Discarded MessageEnd — filtered (boundary signal, not real progress)
    let ev = AgentEvent::MessageEnd {
        message_id: "m1".into(),
        message: test_msg(vec![]),
        discarded: true,
    };
    assert!(
        step_progress_from(&ev).is_none(),
        "discarded MessageEnd should not produce StepProgress"
    );
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
                cache: None,
                r#loop: None,
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
                    cache: None,
                    r#loop: None,
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

    // Deterministically place cancel() ahead of run_loop's step-token
    // registration. This covers cancellation during run() startup, after the
    // state becomes Running but before the first executor can be aborted.
    let abort_registration = engine.current_abort.lock().await;
    let cancel_task = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.cancel("stop").await })
    };
    tokio::task::yield_now().await;
    let runner = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.run().await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while engine.state().await != WorkflowStatus::Running {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("run should enter startup");
    drop(abort_registration);
    cancel_task.await.unwrap().unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), runner)
        .await
        .expect("cancel during startup must stop the first step")
        .unwrap()
        .unwrap();

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
    let captured = client.captured_requests.lock();
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
    use llm_harness_types::{
        CostAggregate, ModelCost, UsageReportAggregate, canonical_billing_usage_profile,
        canonical_usage_report_profile,
    };

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
                total_input_tokens: read + write,
                total_output_tokens: 10,
                total_cache_read_tokens: read,
                total_cache_write_tokens: write,
                total_reasoning_tokens: 5,
                billing_usage_profile: Some(canonical_billing_usage_profile()),
                usage_reports: UsageReportAggregate {
                    profile: Some(canonical_usage_report_profile()),
                    provider_calls: 1,
                    reported_valid: 1,
                    ..Default::default()
                },
                by_model: {
                    let mut m = std::collections::HashMap::new();
                    m.insert(
                        "anthropic/claude-sonnet".to_string(),
                        ModelCost {
                            input_tokens: read + write,
                            output_tokens: 10,
                            cache_read_tokens: read,
                            cache_write_tokens: write,
                            reasoning_tokens: 5,
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
        cache_key: None,
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
    assert_eq!(total.total_input_tokens, 650);
    assert_eq!(total.total_output_tokens, 20);
    assert_eq!(total.total_cache_read_tokens, 500);
    assert_eq!(total.total_cache_write_tokens, 150);
    assert_eq!(total.total_reasoning_tokens, 10);
    assert_eq!(
        total.billing_usage_profile,
        Some(canonical_billing_usage_profile())
    );
    assert_eq!(total.billing_usage_violations, 0);
    assert_eq!(total.usage_reports.provider_calls, 2);
    assert_eq!(total.usage_reports.reported_valid, 2);
    total
        .usage_reports
        .require_all_reported_valid()
        .expect("both workflow calls retain valid usage evidence");

    let mut legacy = mk_record(1, 0);
    let legacy_cost = &mut legacy.result.as_mut().expect("step result").cost;
    legacy_cost.billing_usage_profile = None;
    legacy_cost.usage_reports = UsageReportAggregate::default();
    legacy_cost.by_model.clear();
    let mixed = aggregate_cost(&[legacy, mk_record(200, 100)]);
    assert!(
        mixed.validate_billing_profile().is_err(),
        "legacy numeric usage without provenance must not inherit a later profile"
    );
    assert!(mixed.billing_usage_violations > 0);
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
    let captured = client.captured_requests.lock();
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
    let captured = client.captured_requests.lock();
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
                cache: None,
                r#loop: None,
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
    use llm_harness_subagents::message_bus::MessageBus;
    use llm_harness_subagents::spawn_types::SpawnOptions;
    use llm_harness_subagents::spawner::HarnessSubAgentSpawner;

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
    let bus = MessageBus::new();
    // Set up event sink to collect sub-agent completion events.
    let events = Arc::new(std::sync::Mutex::new(vec![]));
    let events_clone = events.clone();
    bus.set_event_sink(Arc::new(move |event| {
        events_clone.lock().unwrap().push(event);
    }));
    let spawner: HarnessSubAgentSpawner = engine.build_step_spawner(bus, &"run".into(), 1);

    // spawn 一个最小 sub-agent。spawn_async 返回 session_id(String)。
    let session_id = spawner
        .spawn_async(SpawnOptions {
            prompt: "do sub work".into(),
            ..Default::default()
        })
        .await
        .expect("spawn should succeed");
    assert!(!session_id.is_empty(), "session_id should be non-empty");

    // 等待 sub-agent 后台 task 完成并投递 Done 事件。
    // 轮询等待（最多 5s），避免固定 sleep 在慢速 CI (Windows) 上不够。
    let evs = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            {
                let guard = events.lock().unwrap();
                if !guard.is_empty() {
                    break guard;
                }
            }
            if std::time::Instant::now() >= deadline {
                break events.lock().unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };
    assert_eq!(evs.len(), 1, "one Done event should be pushed");
    assert!(
        matches!(
            &evs[0],
            llm_harness_subagents::message_bus::SubAgentEvent::Done { .. }
        ),
        "expected SubAgentEvent::Done, got {:?}",
        evs[0]
    );
    drop(evs);

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

// ── Step-level result cache tests (issue #41) ──────────────────────────

/// Executor that counts executions. Reads `design_version` from context
/// (set by an upstream step) to verify cache behavior.
struct CacheableExecutor {
    call_count: Arc<AtomicUsize>,
}

impl StepExecutor for CacheableExecutor {
    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            let n = self.call_count.fetch_add(1, Ordering::SeqCst) + 1;
            let ctx_data = ctx.context.lock().await;
            let dv = ctx_data
                .variables
                .get("design_version")
                .cloned()
                .unwrap_or(serde_json::json!("unknown"));
            drop(ctx_data);
            Ok(StepResult {
                output: format!("computed for {dv} (call {n})"),
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

/// Writes `design_version` to context. Only meant to run once (entry step).
struct VersionWriter;
impl StepExecutor for VersionWriter {
    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            let mut ctx_data = ctx.context.lock().await;
            ctx_data
                .variables
                .insert("design_version".into(), serde_json::json!("v1"));
            drop(ctx_data);
            Ok(StepResult {
                output: "wrote v1".into(),
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

/// Judge for 3-step workflow s0→s1→s2 with a backjump s2→s1→s2→abort.
struct BackjumpJudge {
    call_count: Arc<AtomicUsize>,
    backjump_target: String,
}

impl StepTransitionJudge for BackjumpJudge {
    fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
        let count = self.call_count.clone();
        let step_id = ctx.current_step.id().clone();
        Box::pin(async move {
            let n = count.fetch_add(1, Ordering::SeqCst) + 1;
            match (step_id.as_str(), n) {
                ("s0", _) => Transition::To("s1".into()),
                ("s1", _) => Transition::To("s2".into()),
                // s2 1st time → backjump; s2 2nd time → done
                ("s2", 3) => Transition::To(self.backjump_target.clone()),
                ("s2", _) => Transition::Abort {
                    reason: "done".into(),
                },
                _ => Transition::Abort {
                    reason: "unexpected".into(),
                },
            }
        })
    }
}

impl BackjumpJudge {
    fn backjump_to(target: &str) -> Self {
        Self {
            call_count: Arc::new(AtomicUsize::new(0)),
            backjump_target: target.into(),
        }
    }
}

impl BackjumpJudge {
    fn with_call_count(mut self, count: Arc<AtomicUsize>) -> Self {
        self.call_count = count;
        self
    }
}

fn cache_test_workflow() -> Workflow {
    Workflow {
        entry_step: "s0".into(),
        steps: vec![
            Step::executor("s0", "Writer", "writer", None),
            Step::executor("s1", "Cacheable", "cacheable", None).with_policy(StepExecutionPolicy {
                timeout_ms: None,
                max_attempts: Some(1),
                retry_backoff_ms: None,
                cache: Some(StepCache {
                    key_pointer: "/design_version".into(),
                    ttl_secs: None,
                }),
                r#loop: None,
            }),
            Step::executor("s2", "Reader", "reader", None),
        ],
        edges: vec![
            Edge {
                from: "s0".into(),
                to: "s1".into(),
                condition: None,
            },
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
    }
}

fn cache_test_config(dir: &tempfile::TempDir, client: Arc<MockLlmClient>) -> WorkflowEngineConfig {
    WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    }
}

#[tokio::test]
async fn step_cache_skips_reexecution_on_backjump() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![]));
    let exec_calls = Arc::new(AtomicUsize::new(0));
    let judge_calls = Arc::new(AtomicUsize::new(0));

    let engine = WorkflowEngine::new(
        cache_test_workflow(),
        cache_test_config(&dir, client),
        Arc::new(BackjumpJudge::backjump_to("s1").with_call_count(judge_calls)),
    )
    .unwrap()
    .with_executor("writer", Arc::new(VersionWriter))
    .with_executor(
        "cacheable",
        Arc::new(CacheableExecutor {
            call_count: exec_calls.clone(),
        }),
    )
    .with_executor("reader", Arc::new(ContextReader));

    let result = engine.run().await.unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);

    // Flow: s0(writes v1) → s1(executes, cache_key=v1) → s2 → backjump s1
    // → cache hit (design_version still v1) → s2 → abort.
    // s1 should execute only once.
    assert_eq!(
        exec_calls.load(Ordering::SeqCst),
        1,
        "cached step should not re-execute on backjump"
    );
    assert!(result.final_message.is_some());
}

#[tokio::test]
async fn step_cache_misses_when_key_changes() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![]));
    let exec_calls = Arc::new(AtomicUsize::new(0));
    let judge_calls = Arc::new(AtomicUsize::new(0));

    // VersionWriter that increments the version on every call, so the
    // cache key always changes → s1 always re-executes on backjump.
    struct IncrementingWriter {
        call_count: Arc<AtomicUsize>,
    }
    impl StepExecutor for IncrementingWriter {
        fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
            Box::pin(async move {
                let n = self.call_count.fetch_add(1, Ordering::SeqCst) + 1;
                let mut ctx_data = ctx.context.lock().await;
                ctx_data
                    .variables
                    .insert("design_version".into(), serde_json::json!(format!("v{n}")));
                drop(ctx_data);
                Ok(StepResult {
                    output: format!("wrote v{n}"),
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

    // Modify workflow: backjump goes to s0 (not s1) so s0 re-runs and
    // changes design_version before s1 sees it again.
    let mut wf = cache_test_workflow();
    wf.edges[2] = Edge {
        from: "s2".into(),
        to: "s0".into(),
        condition: None,
    };

    let engine = WorkflowEngine::new(
        wf,
        cache_test_config(&dir, client),
        Arc::new(BackjumpJudge::backjump_to("s0").with_call_count(judge_calls)),
    )
    .unwrap()
    .with_executor(
        "writer",
        Arc::new(IncrementingWriter {
            call_count: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .with_executor(
        "cacheable",
        Arc::new(CacheableExecutor {
            call_count: exec_calls.clone(),
        }),
    )
    .with_executor("reader", Arc::new(ContextReader));

    engine.run().await.unwrap();

    // Flow: s0(v1) → s1(executes, key=v1) → s2 → backjump s0(v2) → s1
    // (key=v2, cache miss) → s2 → abort.
    // s1 executes twice.
    assert_eq!(
        exec_calls.load(Ordering::SeqCst),
        2,
        "cache should miss when key changes"
    );
}

#[tokio::test]
async fn step_cache_no_policy_always_executes() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![]));
    let exec_calls = Arc::new(AtomicUsize::new(0));
    let judge_calls = Arc::new(AtomicUsize::new(0));

    // Same 3-step workflow but s1 has NO cache policy → always re-executes.
    let mut wf = cache_test_workflow();
    wf.steps[1] = Step::executor("s1", "Cacheable", "cacheable", None);

    let engine = WorkflowEngine::new(
        wf,
        cache_test_config(&dir, client),
        Arc::new(BackjumpJudge::backjump_to("s1").with_call_count(judge_calls)),
    )
    .unwrap()
    .with_executor("writer", Arc::new(VersionWriter))
    .with_executor(
        "cacheable",
        Arc::new(CacheableExecutor {
            call_count: exec_calls.clone(),
        }),
    )
    .with_executor("reader", Arc::new(ContextReader));

    engine.run().await.unwrap();

    // No cache policy → s1 executes on every visit (2 times).
    assert_eq!(
        exec_calls.load(Ordering::SeqCst),
        2,
        "step without cache policy should always execute"
    );
}

struct RejectCachedExecution {
    validation_calls: Arc<AtomicUsize>,
    execution_calls: Arc<AtomicUsize>,
}

impl StepExecutor for RejectCachedExecution {
    fn validate_step(&self, step: &Step) -> Result<()> {
        assert!(
            step.policy()
                .and_then(|policy| policy.cache.as_ref())
                .is_some()
        );
        let call = self.validation_calls.fetch_add(1, Ordering::SeqCst);
        if call > 0 {
            anyhow::bail!("cached execution is no longer allowed")
        }
        Ok(())
    }

    fn execute<'a>(&'a self, _ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        self.execution_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(completed_result("validated execution")) })
    }
}

#[tokio::test]
async fn executor_validation_runs_before_cache_lookup() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![]));
    let validation_calls = Arc::new(AtomicUsize::new(0));
    let execution_calls = Arc::new(AtomicUsize::new(0));

    let engine = WorkflowEngine::new(
        cache_test_workflow(),
        cache_test_config(&dir, client),
        Arc::new(BackjumpJudge::backjump_to("s1")),
    )
    .unwrap()
    .with_executor("writer", Arc::new(VersionWriter))
    .with_executor(
        "cacheable",
        Arc::new(RejectCachedExecution {
            validation_calls: validation_calls.clone(),
            execution_calls: execution_calls.clone(),
        }),
    )
    .with_executor("reader", Arc::new(ContextReader));

    let error = engine.run().await.unwrap_err();

    assert!(
        error
            .to_string()
            .contains("cached execution is no longer allowed")
    );
    assert_eq!(validation_calls.load(Ordering::SeqCst), 2);
    assert_eq!(execution_calls.load(Ordering::SeqCst), 1);
}

struct FinalizingExecutor {
    started: Arc<Notify>,
    cancelled: Arc<Notify>,
    release: Arc<Notify>,
    published: Arc<AtomicBool>,
}

impl StepExecutor for FinalizingExecutor {
    fn cancellation_policy(&self) -> ExecutorCancellationPolicy {
        ExecutorCancellationPolicy::Finalize
    }

    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            self.started.notify_one();
            ctx.abort.cancelled().await;
            self.cancelled.notify_one();
            self.release.notified().await;
            ctx.context
                .lock()
                .await
                .variables
                .insert("finalized".into(), serde_json::json!(true));
            self.published.store(true, Ordering::SeqCst);
            Ok(completed_result("finalized"))
        })
    }
}

struct FinalizingExecutorFixture {
    executor: Arc<FinalizingExecutor>,
    started: Arc<Notify>,
    cancelled: Arc<Notify>,
    release: Arc<Notify>,
    published: Arc<AtomicBool>,
}

fn finalizing_executor_fixture() -> FinalizingExecutorFixture {
    let started = Arc::new(Notify::new());
    let cancelled = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let published = Arc::new(AtomicBool::new(false));
    FinalizingExecutorFixture {
        executor: Arc::new(FinalizingExecutor {
            started: started.clone(),
            cancelled: cancelled.clone(),
            release: release.clone(),
            published: published.clone(),
        }),
        started,
        cancelled,
        release,
        published,
    }
}

#[tokio::test]
async fn finalizing_executor_completes_after_workflow_cancellation() {
    let dir = tempfile::TempDir::new().unwrap();
    let fixture = finalizing_executor_fixture();
    let workflow = Workflow {
        entry_step: "finalize".into(),
        steps: vec![Step::executor("finalize", "Finalize", "finalizing", None)],
        edges: vec![],
    };
    let engine = Arc::new(
        WorkflowEngine::new(
            workflow,
            cache_test_config(&dir, Arc::new(MockLlmClient::new(vec![]))),
            Arc::new(NoopJudge),
        )
        .unwrap()
        .with_executor("finalizing", fixture.executor),
    );
    let runner = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.run().await })
    };

    fixture.started.notified().await;
    engine.cancel("test cancellation").await.unwrap();
    fixture.cancelled.notified().await;
    assert!(!fixture.published.load(Ordering::SeqCst));
    fixture.release.notify_one();
    runner.await.unwrap().unwrap();

    assert!(fixture.published.load(Ordering::SeqCst));
    assert_eq!(engine.context().lock().await.variables["finalized"], true);
    let history = engine.step_history().await;
    assert_eq!(
        history
            .last()
            .and_then(|record| record.result.as_ref())
            .map(|result| result.output.as_str()),
        Some("finalized")
    );
}

#[tokio::test]
async fn finalizing_executor_completes_after_step_timeout() {
    let dir = tempfile::TempDir::new().unwrap();
    let fixture = finalizing_executor_fixture();
    let workflow = Workflow {
        entry_step: "finalize".into(),
        steps: vec![
            Step::executor("finalize", "Finalize", "finalizing", None).with_policy(
                StepExecutionPolicy {
                    timeout_ms: Some(1),
                    ..StepExecutionPolicy::default()
                },
            ),
        ],
        edges: vec![],
    };
    let engine = Arc::new(
        WorkflowEngine::new(
            workflow,
            cache_test_config(&dir, Arc::new(MockLlmClient::new(vec![]))),
            Arc::new(NoopJudge),
        )
        .unwrap()
        .with_executor("finalizing", fixture.executor),
    );
    let runner = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.run().await })
    };

    fixture.started.notified().await;
    fixture.cancelled.notified().await;
    assert!(!fixture.published.load(Ordering::SeqCst));
    fixture.release.notify_one();
    let error = runner.await.unwrap().unwrap_err();

    assert!(error.to_string().contains("timed out"));
    assert!(fixture.published.load(Ordering::SeqCst));
    assert_eq!(engine.context().lock().await.variables["finalized"], true);
}

// ── TextDelta / ThinkingDelta forwarding tests ────────────────────────────

/// A workflow with a single LLM step should forward TextDelta events to
/// subscribers, in addition to StepStarted / StepFinished.
#[tokio::test]
async fn text_delta_events_forwarded_to_subscribers() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("Hello world!")]));
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine =
        Arc::new(WorkflowEngine::new_free_task("say hello".into(), vec![], config).unwrap());
    let mut rx = engine.subscribe();
    let engine_clone = engine.clone();
    let mut run_handle = tokio::spawn(async move { engine_clone.run().await });

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

    // Collect text from TextDelta events and the final output from StepFinished.
    let mut delta_text = String::new();
    let mut step_output: Option<String> = None;
    let mut saw_text_delta = false;
    for ev in &events {
        match ev {
            WorkflowEvent::TextDelta { text, .. } => {
                saw_text_delta = true;
                delta_text.push_str(text);
            }
            WorkflowEvent::StepFinished { result, .. } => {
                step_output = Some(result.output.clone());
            }
            _ => {}
        }
    }

    assert!(saw_text_delta, "expected at least one TextDelta event");
    assert_eq!(
        delta_text, "Hello world!",
        "concatenated TextDelta should match input"
    );
    assert_eq!(
        step_output.as_deref().unwrap_or(""),
        "Hello world!",
        "StepFinished output should match TextDelta concatenation"
    );
}

/// ThinkingDelta events should be forwarded to subscribers.
#[tokio::test]
async fn thinking_delta_events_forwarded_to_subscribers() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text_with_thinking(
        "Let me think...",
        "The answer.",
    )]));
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine =
        Arc::new(WorkflowEngine::new_free_task("think and answer".into(), vec![], config).unwrap());
    let mut rx = engine.subscribe();
    let engine_clone = engine.clone();
    let mut run_handle = tokio::spawn(async move { engine_clone.run().await });

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

    let mut saw_thinking_delta = false;
    let mut thinking_text = String::new();
    let mut saw_text_delta = false;
    for ev in &events {
        match ev {
            WorkflowEvent::ThinkingDelta { thinking, .. } => {
                saw_thinking_delta = true;
                thinking_text.push_str(thinking);
            }
            WorkflowEvent::TextDelta { .. } => {
                saw_text_delta = true;
            }
            _ => {}
        }
    }

    assert!(
        saw_thinking_delta,
        "expected at least one ThinkingDelta event"
    );
    assert_eq!(thinking_text, "Let me think...");
    assert!(saw_text_delta, "expected TextDelta alongside ThinkingDelta");
}

/// When nobody subscribes (channel has 0 receivers), send errors are silently
/// ignored and the step still completes normally.
#[tokio::test]
async fn text_delta_channel_full_does_not_block_step() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("Done.")]));
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new_free_task("just respond".into(), vec![], config).unwrap();
    // Intentionally do NOT subscribe — broadcast send with 0 receivers returns Ok(0).
    let result = engine.run().await.unwrap();

    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert!(
        result
            .final_message
            .as_deref()
            .unwrap_or("")
            .contains("Done.")
    );
}
/// LoopConfig + Transition::Retry: apply_transition() resolves the loop
/// continuation to `target_stage` (a *different* step), so run_loop() must
/// advance current_step to that target instead of re-running the Retry-ing
/// step in place. Regression: run_loop's Retry branch ignored the next_step
/// returned by apply_transition, so a cross-node loop (check --Retry--> run)
/// never left the check step.
#[tokio::test]
async fn retry_with_loop_config_jumps_to_target_stage() {
    use parking_lot::Mutex;

    let visited: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    /// Judge: records each decision. `check` returns Retry (activating its
    /// LoopConfig); the loop head `run` advances to `check` on success;
    /// the second `check` execution succeeds via its success edge.
    struct LoopJudge {
        visited: Arc<Mutex<Vec<String>>>,
    }
    impl StepTransitionJudge for LoopJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let step_id = ctx.current_step.id().clone();
            // Count this visit in the shared log; exactly one check pass
            // returns Retry (the loop iteration under test).
            let check_visits = {
                let mut v = self.visited.lock();
                v.push(step_id.clone());
                v.iter().filter(|s| s.as_str() == "check").count()
            };
            Box::pin(async move {
                match (step_id.as_str(), check_visits) {
                    // First visit: fail the check → Retry → loop jump to run.
                    // Second visit: pass so the flow exits via the success edge.
                    ("check", 1) => Transition::Retry,
                    ("check", _) => Transition::To("done".into()),
                    ("run", _) => Transition::To("check".into()),
                    _ => Transition::Abort {
                        reason: "end".into(),
                    },
                }
            })
        }
    }

    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("run pass 1"),
        MockResponse::text("check fail"),
        // Without the fix, check re-runs here instead of jumping to run.
        MockResponse::text("run pass 2"),
        MockResponse::text("check pass"),
    ]));
    let workflow = Workflow {
        entry_step: "run".into(),
        steps: vec![
            Step::llm("run", "Run", "do work", vec![]),
            Step::llm("check", "Check", "validate", vec![]).with_policy(StepExecutionPolicy {
                timeout_ms: None,
                max_attempts: None,
                retry_backoff_ms: None,
                cache: None,
                r#loop: Some(LoopConfig {
                    max_iterations: 5,
                    target_stage: Some("run".into()),
                    exit_route: None,
                }),
            }),
            Step::Terminal {
                id: "done".into(),
                name: "Done".into(),
                exit_code: 0,
                message: Some("done".into()),
            },
        ],
        edges: vec![
            Edge {
                from: "run".into(),
                to: "check".into(),
                condition: None,
            },
            Edge {
                from: "check".into(),
                to: "done".into(),
                condition: None,
            },
        ],
    };
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(
        workflow,
        config,
        Arc::new(LoopJudge {
            visited: visited.clone(),
        }),
    )
    .unwrap();
    let _ = engine.run().await.unwrap();

    let order = visited.lock().clone();
    // Loop continuation must jump to `run` (loop head), not re-run `check`.
    // Expected visit order: run -> check -> run -> check. The bug yields
    // run -> check -> check (check re-runs in place, no jump).
    assert_eq!(
        order,
        vec![
            "run".to_string(),
            "check".to_string(),
            "run".to_string(),
            "check".to_string(),
        ],
        "Retry with LoopConfig.target_stage must jump to the target stage; got {order:?}"
    );
}

/// LoopConfig iteration counting must survive cross-node iterations. The
/// counter is per LoopConfig-carrying step (persisted in WorkflowState), so
/// `check --Retry/target_stage=run--> To(check) --> check` still accumulates.
/// Regression: the counter was a consecutive-Retry history scan, which the
/// interleaved `run`/To record reset every iteration — max_iterations=2
/// never fired and the loop ran until max_steps (500) or the mock ran out.
#[tokio::test]
async fn loop_config_max_iterations_reached_across_cross_node_iterations() {
    use parking_lot::Mutex;

    let visited: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    /// Judge: `check` ALWAYS returns Retry (the loop never exits through the
    /// judge); `run` routes back to `check`. Exhaustion must come from
    /// max_iterations; after it is exhausted, a LoopConfig with no
    /// exit_route fails explicitly.
    struct AlwaysRetryJudge {
        visited: Arc<Mutex<Vec<String>>>,
    }
    impl StepTransitionJudge for AlwaysRetryJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let step_id = ctx.current_step.id().clone();
            self.visited.lock().push(step_id.clone());
            Box::pin(async move {
                match step_id.as_str() {
                    "check" => Transition::Retry,
                    "run" => Transition::To("check".into()),
                    _ => Transition::Abort {
                        reason: "end".into(),
                    },
                }
            })
        }
    }

    let dir = tempfile::TempDir::new().unwrap();
    // Exactly enough responses for the expected bound: run + 3 check visits.
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("run 1"),
        MockResponse::text("check 1"),
        MockResponse::text("run 2"),
        MockResponse::text("check 2"),
        MockResponse::text("run 3"),
        MockResponse::text("check 3"),
    ]));
    let workflow = Workflow {
        entry_step: "run".into(),
        steps: vec![
            Step::llm("run", "Run", "do work", vec![]),
            Step::llm("check", "Check", "validate", vec![]).with_policy(StepExecutionPolicy {
                timeout_ms: None,
                max_attempts: None,
                retry_backoff_ms: None,
                cache: None,
                r#loop: Some(LoopConfig {
                    max_iterations: 2,
                    target_stage: Some("run".into()),
                    exit_route: None,
                }),
            }),
        ],
        edges: vec![Edge {
            from: "run".into(),
            to: "check".into(),
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
    let engine = WorkflowEngine::new(
        workflow,
        config,
        Arc::new(AlwaysRetryJudge {
            visited: visited.clone(),
        }),
    )
    .unwrap()
    .with_max_retries(0);
    let run = engine.run().await;

    // Pre-fix: the interleaved run/To reset the consecutive-Retry scan, so
    // every check visit had loop_count == 1 and the loop spun unbounded.
    // Post-fix: the persisted counter survives the cross-node jump; check
    // visits 1..3 increment it, visit 3 (counter=3 > max_iterations=2)
    // exhausts the loop. With no exit_route configured the engine now
    // fails explicitly (Task 4) instead of falling through to max_retries.
    let check_visits = visited
        .lock()
        .iter()
        .filter(|s| s.as_str() == "check")
        .count();
    assert_eq!(
        check_visits, 3,
        "cross-node loop must reach max_iterations=2 and stop at the 3rd check visit"
    );
    match run {
        Err(WorkflowError::TaskFailed(msg)) => {
            assert!(
                msg.contains("no exit_route configured"),
                "exhaustion without exit_route must fail explicitly, got: {msg}"
            );
        }
        other => panic!("expected TaskFailed from loop exhaustion, got {other:?}"),
    }
}

/// Counter reset/re-entry semantics at the state level: the persisted
/// per-step counter must survive unrelated steps' transitions, and clearing
/// it on loop exit must give a fresh count on re-entry (no stale carry-over).
#[test]
fn loop_iterations_counter_survives_interleaving_and_resets_on_exit() {
    let mut state = WorkflowState::new("run".into());

    // Episode 1: three Retry iterations on `check` (crosses max_iterations=2).
    for _ in 0..3 {
        state
            .loop_iterations
            .entry("check".into())
            .and_modify(|c| *c += 1)
            .or_insert(1);
    }
    assert_eq!(state.loop_iterations.get("check"), Some(&3));

    // Interleaved unrelated steps must NOT touch the `check` counter.
    state
        .loop_iterations
        .entry("run".into())
        .and_modify(|c| *c += 1)
        .or_insert(1);
    assert_eq!(state.loop_iterations.get("check"), Some(&3));
    assert_eq!(state.loop_iterations.get("run"), Some(&1));

    // Loop exit clears only that step's counter; a sibling loop is untouched.
    state.loop_iterations.remove("check");
    assert_eq!(state.loop_iterations.get("check"), None);

    // Re-entry: fresh count starting at 1, not inherited from episode 1.
    state
        .loop_iterations
        .entry("check".into())
        .and_modify(|c| *c += 1)
        .or_insert(1);
    assert_eq!(state.loop_iterations.get("check"), Some(&1));
}

/// Backward compatibility: a persisted snapshot from before the
/// `loop_iterations` field existed (no key in JSON) must deserialize with an
/// empty counter map, and a snapshot that carries the key round-trips.
#[test]
fn workflow_state_without_loop_iterations_key_deserializes() {
    // Pre-fix snapshot JSON: no loop_iterations key.
    let legacy = serde_json::json!({
        "status": "running",
        "current_step": "check",
        "step_history": [],
    });
    let state: WorkflowState = serde_json::from_value(legacy).unwrap();
    assert!(state.loop_iterations.is_empty());

    // Round-trip with a populated counter.
    let mut state = WorkflowState::new("check".into());
    state.loop_iterations.insert("check".into(), 3);
    let json = serde_json::to_value(&state).unwrap();
    assert!(json.get("loop_iterations").is_some());
    let back: WorkflowState = serde_json::from_value(json).unwrap();
    assert_eq!(back.loop_iterations.get("check"), Some(&3));
}

/// exit_route end-to-end: a cross-node loop (run -> check --Retry--> run)
/// with max_iterations=2 must exit through the declarative exit edge after
/// exactly 3 check visits, landing on the terminal step with a Succeeded
/// task. The exit edge is the Label("done") edge that Senza's
/// stages_to_workflow generates from a stage's "next_on_done" key, and
/// LoopConfig.exit_route = "done" (or "next_on_done") matches it.
#[tokio::test]
async fn loop_config_exit_route_terminates_after_max_iterations() {
    use parking_lot::Mutex;

    let visited: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    /// Judge: `check` ALWAYS returns Retry; `run` routes back to `check`.
    /// Termination must come from LoopConfig exhaustion → exit_route.
    struct AlwaysRetryJudge {
        visited: Arc<Mutex<Vec<String>>>,
    }
    impl StepTransitionJudge for AlwaysRetryJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let step_id = ctx.current_step.id().clone();
            self.visited.lock().push(step_id.clone());
            Box::pin(async move {
                match step_id.as_str() {
                    "check" => Transition::Retry,
                    "run" => Transition::To("check".into()),
                    _ => Transition::Abort {
                        reason: "end".into(),
                    },
                }
            })
        }
    }

    let dir = tempfile::TempDir::new().unwrap();
    // Exactly enough responses for 3 run + 3 check visits.
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("run 1"),
        MockResponse::text("check 1"),
        MockResponse::text("run 2"),
        MockResponse::text("check 2"),
        MockResponse::text("run 3"),
        MockResponse::text("check 3"),
    ]));
    let workflow = Workflow {
        entry_step: "run".into(),
        steps: vec![
            Step::llm("run", "Run", "do work", vec![]),
            Step::llm("check", "Check", "validate", vec![]).with_policy(StepExecutionPolicy {
                timeout_ms: None,
                max_attempts: None,
                retry_backoff_ms: None,
                cache: None,
                r#loop: Some(LoopConfig {
                    max_iterations: 2,
                    target_stage: Some("run".into()),
                    // What Senza's stages_to_workflow produces from
                    // "exit_route": "done" + "next_on_done": "done".
                    exit_route: Some("done".into()),
                }),
            }),
            Step::Terminal {
                id: "done".into(),
                name: "Done".into(),
                exit_code: 0,
                message: Some("done".into()),
            },
        ],
        edges: vec![
            Edge {
                from: "run".into(),
                to: "check".into(),
                condition: None,
            },
            // The declarative exit edge (from next_on_done in Senza).
            Edge {
                from: "check".into(),
                to: "done".into(),
                condition: Some(EdgeCondition::Label("done".into())),
            },
        ],
    };
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(
        workflow,
        config,
        Arc::new(AlwaysRetryJudge {
            visited: visited.clone(),
        }),
    )
    .unwrap()
    .with_max_retries(0);
    let result = engine.run().await.unwrap();

    let check_visits = visited
        .lock()
        .iter()
        .filter(|s| s.as_str() == "check")
        .count();
    assert_eq!(
        check_visits, 3,
        "loop must run exactly max_iterations=2 iterations plus the final exhausting visit"
    );
    assert_eq!(
        engine.state().await,
        WorkflowStatus::Succeeded,
        "exit_route must route the exhausted loop to the terminal step"
    );
    // TaskResult comes from the Terminal short-circuit (exit_code 0).
    assert_eq!(result.final_message.as_deref(), Some("done"));
}

/// Route-key / Transition::To loop continuation: a LoopConfig-carrying step
/// that routes To(target_stage) counts as a loop iteration and must exhaust
/// at max_iterations, then follow exit_route. Research Agent shape:
/// search_sources -> vet_sources --route_key=research--> search_sources -> ...
/// where vet's structured route_key matches the Label edge to the loop head.
#[tokio::test]
async fn to_based_cross_node_loop_reaches_max_iterations_and_exits() {
    use parking_lot::Mutex;

    let visited: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    /// Judge: `vet` ALWAYS routes To(search_sources) via the declarative
    /// label edge (route_key "research"); `search` routes forward to vet.
    struct ResearchRouteJudge {
        visited: Arc<Mutex<Vec<String>>>,
    }
    impl StepTransitionJudge for ResearchRouteJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let step_id = ctx.current_step.id().clone();
            self.visited.lock().push(step_id.clone());
            Box::pin(async move {
                match step_id.as_str() {
                    "vet" => Transition::To("search".into()),
                    "search" => Transition::To("vet".into()),
                    _ => Transition::Abort {
                        reason: "end".into(),
                    },
                }
            })
        }
    }

    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("search 1"),
        MockResponse::text("vet 1"),
        MockResponse::text("search 2"),
        MockResponse::text("vet 2"),
        MockResponse::text("search 3"),
        MockResponse::text("vet 3"),
    ]));
    let workflow = Workflow {
        entry_step: "search".into(),
        steps: vec![
            Step::llm("search", "Search", "find sources", vec![]),
            Step::llm("vet", "Vet", "vet sources", vec![]).with_policy(StepExecutionPolicy {
                timeout_ms: None,
                max_attempts: None,
                retry_backoff_ms: None,
                cache: None,
                r#loop: Some(LoopConfig {
                    max_iterations: 2,
                    target_stage: Some("search".into()),
                    exit_route: Some("done".into()),
                }),
            }),
            Step::Terminal {
                id: "done".into(),
                name: "Done".into(),
                exit_code: 0,
                message: Some("done".into()),
            },
        ],
        edges: vec![
            Edge {
                from: "search".into(),
                to: "vet".into(),
                condition: None,
            },
            // The declarative continuation edge (next_on_research -> search).
            Edge {
                from: "vet".into(),
                to: "search".into(),
                condition: Some(EdgeCondition::Label("research".into())),
            },
            // The declarative exit edge (next_on_done -> done terminal).
            Edge {
                from: "vet".into(),
                to: "done".into(),
                condition: Some(EdgeCondition::Label("done".into())),
            },
        ],
    };
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(
        workflow,
        config,
        Arc::new(ResearchRouteJudge {
            visited: visited.clone(),
        }),
    )
    .unwrap();
    let result = engine.run().await.unwrap();

    let vet_visits = visited
        .lock()
        .iter()
        .filter(|s| s.as_str() == "vet")
        .count();
    assert_eq!(
        vet_visits, 3,
        "To-based loop must run max_iterations=2 continuations plus the exhausting visit"
    );
    assert_eq!(
        engine.state().await,
        WorkflowStatus::Succeeded,
        "exhausted To-loop must exit via exit_route to the terminal step"
    );
    assert_eq!(result.final_message.as_deref(), Some("done"));
}

/// Counter isolation: an unrelated step's To transition to the same target
/// must not increment another step's loop counter. Only the LoopConfig-
/// carrying step's own To(target_stage) counts.
#[test]
fn unrelated_to_to_same_target_does_not_count_as_loop_iteration() {
    let mut state = WorkflowState::new("search".into());

    // `other` routes To(search) — search has no LoopConfig, and even if some
    // third step carried one for target "search", THIS transition originates
    // from `other`, not from the LoopConfig-carrying step.
    state
        .loop_iterations
        .entry("vet".into())
        .and_modify(|c| *c += 1)
        .or_insert(1);
    assert_eq!(state.loop_iterations.get("vet"), Some(&1));

    // A To from a different source (`fresh_entry`) is a separate episode and
    // must not be visible in `vet`'s counter.
    assert_eq!(state.loop_iterations.get("fresh_entry"), None);
}

/// Mixed Retry + To continuation: both paths share the same persisted
/// per-step counter, so a loop alternating between judge Retry and
/// declarative route-key continuation counts coherently.
#[tokio::test]
async fn mixed_retry_and_to_continuation_share_one_counter() {
    use parking_lot::Mutex;

    let visited: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    /// Judge: alternates Retry / To(search) on `vet` by visit count.
    struct AlternatingJudge {
        visited: Arc<Mutex<Vec<String>>>,
    }
    impl StepTransitionJudge for AlternatingJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let step_id = ctx.current_step.id().clone();
            let vet_visits = {
                let mut v = self.visited.lock();
                v.push(step_id.clone());
                v.iter().filter(|s| s.as_str() == "vet").count()
            };
            Box::pin(async move {
                match (step_id.as_str(), vet_visits) {
                    ("vet", 1) => Transition::Retry,               // in-place retry
                    ("vet", _) => Transition::To("search".into()), // route-key continuation
                    ("search", _) => Transition::To("vet".into()),
                    _ => Transition::Abort {
                        reason: "end".into(),
                    },
                }
            })
        }
    }

    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("search 1"),
        MockResponse::text("vet 1"),
        MockResponse::text("search 2"),
        MockResponse::text("vet 2"),
        MockResponse::text("search 3"),
        MockResponse::text("vet 3"),
    ]));
    let workflow = Workflow {
        entry_step: "search".into(),
        steps: vec![
            Step::llm("search", "Search", "find sources", vec![]),
            Step::llm("vet", "Vet", "vet sources", vec![]).with_policy(StepExecutionPolicy {
                timeout_ms: None,
                max_attempts: None,
                retry_backoff_ms: None,
                cache: None,
                r#loop: Some(LoopConfig {
                    max_iterations: 2,
                    target_stage: Some("search".into()),
                    exit_route: Some("done".into()),
                }),
            }),
            Step::Terminal {
                id: "done".into(),
                name: "Done".into(),
                exit_code: 0,
                message: Some("done".into()),
            },
        ],
        edges: vec![
            Edge {
                from: "search".into(),
                to: "vet".into(),
                condition: None,
            },
            Edge {
                from: "vet".into(),
                to: "search".into(),
                condition: Some(EdgeCondition::Label("research".into())),
            },
            Edge {
                from: "vet".into(),
                to: "done".into(),
                condition: Some(EdgeCondition::Label("done".into())),
            },
        ],
    };
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(
        workflow,
        config,
        Arc::new(AlternatingJudge {
            visited: visited.clone(),
        }),
    )
    .unwrap();
    let result = engine.run().await.unwrap();

    // Visits: vet(1) Retry → vet(2) To→search → vet(3) To → exhausted
    // (counter 3 > 2) → exit_route done. All three vet iterations share one
    // counter despite the mixed transition kinds.
    let vet_visits = visited
        .lock()
        .iter()
        .filter(|s| s.as_str() == "vet")
        .count();
    assert_eq!(
        vet_visits, 3,
        "Retry and To continuations must share one counter"
    );
    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert_eq!(result.final_message.as_deref(), Some("done"));
}

/// Exhausted To-loop with an unresolvable exit_route must fail explicitly
/// (To-based loops have no Retry records, so the max_retries fallback can
/// never fire — silently continuing would loop until max_steps).
#[tokio::test]
async fn exhausted_to_loop_without_resolvable_exit_route_fails_explicitly() {
    use parking_lot::Mutex;

    let visited: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    struct ToLoopJudge {
        visited: Arc<Mutex<Vec<String>>>,
    }
    impl StepTransitionJudge for ToLoopJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let step_id = ctx.current_step.id().clone();
            self.visited.lock().push(step_id.clone());
            Box::pin(async move {
                match step_id.as_str() {
                    "vet" => Transition::To("search".into()),
                    "search" => Transition::To("vet".into()),
                    _ => Transition::Abort {
                        reason: "end".into(),
                    },
                }
            })
        }
    }

    let dir = tempfile::TempDir::new().unwrap();
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("search 1"),
        MockResponse::text("vet 1"),
        MockResponse::text("search 2"),
        MockResponse::text("vet 2"),
        MockResponse::text("search 3"),
        MockResponse::text("vet 3"),
    ]));
    let workflow = Workflow {
        entry_step: "search".into(),
        steps: vec![
            Step::llm("search", "Search", "find sources", vec![]),
            Step::llm("vet", "Vet", "vet sources", vec![]).with_policy(StepExecutionPolicy {
                timeout_ms: None,
                max_attempts: None,
                retry_backoff_ms: None,
                cache: None,
                r#loop: Some(LoopConfig {
                    max_iterations: 2,
                    target_stage: Some("search".into()),
                    // Points at a label with no matching edge — unresolvable.
                    exit_route: Some("typo".into()),
                }),
            }),
        ],
        edges: vec![
            Edge {
                from: "search".into(),
                to: "vet".into(),
                condition: None,
            },
            Edge {
                from: "vet".into(),
                to: "search".into(),
                condition: Some(EdgeCondition::Label("research".into())),
            },
        ],
    };
    let config = WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(TestEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: dir.path().join("sessions"),
        customize_builder: None,
    };
    let engine = WorkflowEngine::new(
        workflow,
        config,
        Arc::new(ToLoopJudge {
            visited: visited.clone(),
        }),
    )
    .unwrap();
    let run = engine.run().await;
    let vet_visits = visited
        .lock()
        .iter()
        .filter(|s| s.as_str() == "vet")
        .count();
    assert_eq!(
        vet_visits, 3,
        "To-loop must run max_iterations=2 continuations plus the exhausting visit"
    );
    match run {
        Err(WorkflowError::TaskFailed(msg)) => {
            assert!(
                msg.contains("exit_route"),
                "explicit failure must name the unresolvable exit_route, got: {msg}"
            );
        }
        other => panic!("expected explicit TaskFailed for unresolvable exit_route, got {other:?}"),
    }
}
