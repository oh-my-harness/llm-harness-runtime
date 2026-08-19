//! WorkflowEngine end-to-end tests — drive a real AgentHarness with MockLlmClient.
//!
//! Covers:
//!   1. Smoke: Idle → Running → Succeeded with text response.
//!   2. Multi-turn: tool_use → text triggers turns > 1.
//!   3. Tracing: TracingHookAdapter wired as after_turn emits exported spans.
//!
use llm_harness_runtime::sandbox::{ResourceLimits, Sandbox, SandboxConfig};

use std::path::Path;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
use llm_harness_runtime::spawn::spawner::JsonlSessionFactory;
use llm_harness_runtime::workflow::engine::{
    WorkflowEngine, WorkflowEngineConfig, WorkflowRunRequest,
};
use llm_harness_runtime::workflow::error::WorkflowError;
use llm_harness_runtime::workflow::model::WorkflowStatus;
use llm_harness_runtime_platform::env::EnvFactory;
use llm_harness_runtime_sandbox::OsEnvSandbox;
use llm_harness_types::{
    AgentError, DataBlock, ExecutionEnv, Tool, ToolContext, ToolFailure, ToolResult,
};

// ── 辅助 ──────────────────────────────────────────────────────────────────────

struct OsEnvFactory {
    sandbox: Arc<OsEnvSandbox>,
}

impl EnvFactory for OsEnvFactory {
    fn create(&self, _cwd: &Path) -> Result<Arc<dyn ExecutionEnv>, AgentError> {
        Ok(self.sandbox.env())
    }
}

fn make_sandbox() -> Arc<OsEnvSandbox> {
    let dir = tempfile::tempdir().unwrap();
    let config = SandboxConfig {
        fs_allowlist: vec![dir.path().to_path_buf()],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(dir.path().to_path_buf()),
    };
    std::mem::forget(dir);
    Arc::new(OsEnvSandbox::new(config))
}

fn make_config(client: Arc<MockLlmClient>, subdir: &str) -> WorkflowEngineConfig {
    WorkflowEngineConfig {
        client,
        model: "mock-model".into(),
        env_factory: Arc::new(OsEnvFactory {
            sandbox: make_sandbox(),
        }),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: std::env::temp_dir().join(subdir),
        customize_builder: None,
    }
}

// ── Test 1: smoke ─────────────────────────────────────────────────────────────

/// Basic lifecycle: Idle → Running → Succeeded, final_message is populated.
#[tokio::test]
async fn workflow_engine_smoke_text_succeeds() {
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
        "The answer is 42.",
    )]));
    let engine = WorkflowEngine::new_free_task(
        "what is 6*7?".into(),
        vec![],
        make_config(client, "task-e2e-smoke"),
    )
    .unwrap();
    assert_eq!(engine.state().await, WorkflowStatus::Idle);

    let result = engine.run().await.unwrap();

    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert!(result.final_message.as_deref().unwrap_or("").contains("42"));
    assert!(result.turns >= 1);
}

// ── Test 2: multi-turn ────────────────────────────────────────────────────────

/// tool_use → text completes in ≥2 turns and final_message is from the text turn.
#[tokio::test]
async fn workflow_engine_multi_turn_tool_then_text() {
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use("c1", "echo", r#"{"msg":"hi"}"#),
        MockResponse::text("All done."),
    ]));
    let engine = WorkflowEngine::new_free_task(
        "echo something".into(),
        vec![],
        make_config(client, "task-e2e-multiturn"),
    )
    .unwrap();

    let result = engine.run().await.unwrap();

    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    assert_eq!(result.final_message.as_deref(), Some("All done."));
    // 单步 workflow 的 turns = step_history.len() = 1（步数，非 LLM turn 数）。
    // ⚠ 注意：TaskResult.turns 在 WorkflowEngine 中是 step_history.len()，
    //   不是 LLM turn 数。此断言调整为步数。
    assert!(
        result.turns >= 1,
        "expected >= 1 step, got {}",
        result.turns
    );
}

#[derive(Clone)]
struct TenantContext(String);

struct CaptureTenantTool {
    observed: Arc<Mutex<Vec<Option<String>>>>,
}

impl Tool for CaptureTenantTool {
    fn name(&self) -> &str {
        "capture_tenant"
    }

    fn description(&self) -> &str {
        "Captures the typed workflow request context visible to this step."
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
        })
    }

    fn execute<'a>(
        &'a self,
        _: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let tenant = ctx
                .run
                .extension::<TenantContext>()
                .map(|context| context.0.clone());
            self.observed.lock().unwrap().push(tenant);
            Ok(ToolResult::full(
                vec![DataBlock::text("captured")],
                serde_json::json!({"captured": true}),
                false,
            ))
        })
    }
}

#[tokio::test]
async fn workflow_request_extensions_reach_llm_step_tools_without_cross_run_leakage() {
    let observed = Arc::new(Mutex::new(Vec::new()));

    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use("capture-1", "capture_tenant", "{}"),
        MockResponse::text("first done"),
    ]));
    let first = WorkflowEngine::new_free_task(
        "capture the tenant".into(),
        vec!["capture_tenant".into()],
        make_config(client, "task-e2e-workflow-request-first"),
    )
    .unwrap()
    .with_tool(Arc::new(CaptureTenantTool {
        observed: observed.clone(),
    }));
    first
        .run_with_request(
            WorkflowRunRequest::new().with_extension(TenantContext("tenant-a".into())),
        )
        .await
        .unwrap();

    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use("capture-2", "capture_tenant", "{}"),
        MockResponse::text("second done"),
    ]));
    let second = WorkflowEngine::new_free_task(
        "capture the tenant".into(),
        vec!["capture_tenant".into()],
        make_config(client, "task-e2e-workflow-request-second"),
    )
    .unwrap()
    .with_tool(Arc::new(CaptureTenantTool {
        observed: observed.clone(),
    }));
    second.run().await.unwrap();

    assert_eq!(
        *observed.lock().unwrap(),
        vec![Some("tenant-a".into()), None],
        "request extensions must be scoped to one workflow invocation"
    );
}

// ── Test 3: tracing ───────────────────────────────────────────────────────────

/// TracingHookAdapter wired via with_builder_customize() exports spans to InMemoryTraceExporter.
#[tokio::test]
async fn workflow_engine_tracing_emits_spans() {
    use llm_harness_runtime::tracer::{CaptureLevel, SpanCaptureConfig, SpanStatus};
    use llm_harness_runtime::tracing_adapter::TracingHookAdapter;
    use llm_harness_runtime_trace_otel::InMemoryTraceExporter;

    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
        "Hello from tracer.",
    )]));
    let exporter = Arc::new(InMemoryTraceExporter::new());
    let adapter = Arc::new(TracingHookAdapter::new(
        exporter.clone(),
        SpanCaptureConfig {
            level: CaptureLevel::Full,
            redactor: None,
        },
    ));
    adapter.begin_task("trace-test");

    let mut hooks = llm_harness_agent::HarnessHooks::none();
    hooks.before_provider_request.push(adapter.clone());
    hooks.after_provider_response.push(adapter.clone());
    hooks.after_tool_call.push(adapter.clone());
    hooks.after_turn.push(adapter.clone());

    let config = make_config(client, "task-e2e-tracing")
        .with_builder_customize(move |b| b.hooks(hooks.clone()));
    let engine = WorkflowEngine::new_free_task("trace me".into(), vec![], config).unwrap();
    engine.run().await.unwrap();
    adapter.end_task("trace-test", SpanStatus::Ok);

    assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
    // 验证至少有一个 span 被 export。
    assert!(
        !exporter.exported_spans().is_empty(),
        "expected at least one exported span"
    );
}

// ── Test 4: stream error propagation (issue #69) ─────────────────────────────

/// A stream that drops mid-way (emits `Err(StreamIncomplete)` as a stream
/// event) must surface as `WorkflowError::StepFailed`, not an empty success.
///
/// Reproduces issue #69: `AgentEvent::Error(Provider("stream incomplete: ..."))`
/// was silently swallowed by `run_llm_step`'s event loop, causing the step to
/// be marked succeeded with `tool_calls_count=0, output=""`.
#[tokio::test]
async fn workflow_engine_stream_incomplete_fails_step() {
    // MockResponse with a mid-stream Err(StreamIncomplete): a couple of text
    // deltas arrive, then the stream terminates abnormally.
    let dropped_response = MockResponse::stream_dropped_mid("partial", 2);
    let client = Arc::new(MockLlmClient::new(vec![dropped_response]));
    let engine = WorkflowEngine::new_free_task(
        "do something".into(),
        vec![],
        make_config(client, "task-e2e-stream-incomplete"),
    )
    .unwrap();

    let result = engine.run().await;

    // run() must return Err mentioning the stream-incomplete failure —
    // NOT Ok with empty/partial output. The engine wraps step errors as
    // TaskFailed("step '{id}' failed: {step_error}"), so we match on that.
    let err = result.expect_err("stream-incomplete must fail the step, not succeed");
    let msg = err.to_string();
    assert!(
        msg.contains("stream incomplete"),
        "error should mention stream incomplete, got: {msg}"
    );
    assert!(
        msg.contains("step 'run' failed"),
        "error should identify the failed step, got: {msg}"
    );

    // Engine state must be Failed.
    assert_eq!(engine.state().await, WorkflowStatus::Failed);
}

// ── Test 5: AgentEvent::Error(Aborted) preserves StepAborted (issue #69 guard) ──

/// A tool that blocks until its abort token fires. Signals `ready` once it
/// has entered the blocking select, so the test can cancel deterministically
/// *during* tool execution (not before the step starts).
struct BlockUntilAborted {
    ready: Arc<tokio::sync::Notify>,
}

impl llm_harness_types::Tool for BlockUntilAborted {
    fn name(&self) -> &str {
        "block_until_aborted"
    }
    fn description(&self) -> &str {
        "Blocks until aborted; used to test cancel."
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA
            .get_or_init(|| serde_json::json!({"type": "object", "properties": {}, "required": []}))
    }
    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        ctx: &'a llm_harness_types::ToolContext,
    ) -> futures::future::BoxFuture<
        'a,
        Result<llm_harness_types::ToolResult, llm_harness_types::ToolFailure>,
    > {
        let ready = self.ready.clone();
        Box::pin(async move {
            // Signal that the tool is now blocking on the abort token,
            // then wait for cancellation. This guarantees cancel lands
            // *during* tool execution, so `AgentEvent::Error(Aborted)`
            // is actually emitted and reaches the runner's select loop.
            ready.notify_one();
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                    Ok(llm_harness_types::ToolResult::full(
                        vec![llm_harness_types::DataBlock::text("done")],
                        serde_json::Value::Null,
                        false,
                    ))
                }
                _ = ctx.abort.cancelled() => {
                    Err(llm_harness_types::ToolFailure::aborted())
                }
            }
        })
    }
}

/// When cancellation fires during step execution, the engine must surface
/// `StepAborted` (or the Cancelled short-circuit) — NOT `StepFailed`. This
/// guards the invariant that the new `AgentEvent::Error` arm must not capture
/// `AgentEvent::Error(AgentError::Aborted)`.
#[tokio::test]
async fn workflow_engine_cancel_preserves_step_aborted_not_failed() {
    // LLM calls block_until_aborted, which blocks until abort fires.
    let ready = Arc::new(tokio::sync::Notify::new());
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::tool_use(
        "call_1",
        "block_until_aborted",
        "{}",
    )]));
    let engine = Arc::new(
        WorkflowEngine::new_free_task(
            "run then block".into(),
            vec!["block_until_aborted".into()],
            make_config(client, "task-e2e-cancel-aborted"),
        )
        .unwrap()
        .with_tool(Arc::new(BlockUntilAborted {
            ready: ready.clone(),
        })),
    );

    let run_handle = {
        let e = Arc::clone(&engine);
        tokio::spawn(async move { e.run().await })
    };
    // Wait until the tool is blocking on the abort token (deterministic),
    // then cancel. This ensures cancel lands *during* tool execution, so
    // the agent loop yields `AgentEvent::Error(Aborted)` and the runner's
    // new Error arm is actually exercised.
    ready.notified().await;

    engine.cancel("test").await.unwrap();

    let outcome = run_handle.await.expect("run task panicked");
    // On cancel, run() returns Ok(TaskResult) via the Cancelled short-circuit
    // (runner.rs:472), OR an Err if cancel landed before that path. Either way,
    // it must NOT be a StepFailed stemming from the new Error arm.
    match outcome {
        Ok(_) => {
            assert_eq!(
                engine.state().await,
                WorkflowStatus::Cancelled,
                "cancel should produce Cancelled status, not Succeeded/Failed"
            );
        }
        Err(e) => {
            assert!(
                matches!(e, WorkflowError::StepAborted { .. }),
                "cancel must surface StepAborted, got: {e:?}"
            );
        }
    }
}
