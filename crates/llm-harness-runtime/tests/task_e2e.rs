//! TaskRunner end-to-end tests — drive a real AgentHarness with MockLlmClient.
//!
//! Covers:
//!   1. Smoke: Pending → Running → Succeeded with text response.
//!   2. Multi-turn: tool_use → text triggers turns > 1.
//!   3. Tracing: TracingHookAdapter wired as after_turn emits exported spans.

use std::sync::Arc;

use llm_harness_agent::HarnessHooks;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
use llm_harness_runtime::TracingHookAdapter;
use llm_harness_runtime::sandbox::{ResourceLimits, SandboxConfig};
use llm_harness_runtime::task::{RetryPolicy, TaskRunnerImpl, TaskState};
use llm_harness_runtime::tracer::SpanCaptureConfig;
use llm_harness_runtime::{TaskId, TaskRunner};
use llm_harness_runtime_sandbox_os::OsEnvSandbox;
use llm_harness_runtime_trace_otel::InMemoryTraceExporter;
use llm_harness_types::AfterTurnHook;

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

fn make_runner_with_hooks(responses: Vec<MockResponse>, hooks: HarnessHooks) -> TaskRunnerImpl {
    let client = Arc::new(MockLlmClient::new(responses));
    TaskRunnerImpl::new(
        TaskId("test-task".into()),
        make_sandbox(),
        None,
        RetryPolicy::default(),
        None,
        None,
    )
    .with_harness(client, "mock-model")
    .with_hooks(hooks)
}

fn make_runner(responses: Vec<MockResponse>) -> TaskRunnerImpl {
    make_runner_with_hooks(responses, HarnessHooks::none())
}

// ── Test 1: smoke ─────────────────────────────────────────────────────────────

/// Basic lifecycle: Pending → Running → Succeeded, final_message is populated.
#[tokio::test]
async fn task_runner_smoke_text_succeeds() {
    let runner = make_runner(vec![MockResponse::text("The answer is 42.")]);
    assert!(matches!(runner.state(), TaskState::Pending));

    runner.start("what is 6*7?").await.unwrap();

    match runner.state() {
        TaskState::Succeeded { result, .. } => {
            assert_eq!(result.final_message.as_deref(), Some("The answer is 42."));
            assert!(result.turns >= 1);
        }
        other => panic!("unexpected state: {other:?}"),
    }
}

// ── Test 2: multi-turn ────────────────────────────────────────────────────────

/// tool_use → text completes in ≥2 turns and final_message is from the text turn.
#[tokio::test]
async fn task_runner_multi_turn_tool_then_text() {
    let runner = make_runner(vec![
        MockResponse::tool_use("c1", "echo", r#"{"msg":"hi"}"#),
        MockResponse::text("All done."),
    ]);

    runner.start("echo something").await.unwrap();

    match runner.state() {
        TaskState::Succeeded { result, .. } => {
            assert_eq!(result.final_message.as_deref(), Some("All done."));
            assert!(
                result.turns >= 2,
                "expected >= 2 turns, got {}",
                result.turns
            );
        }
        other => panic!("unexpected state: {other:?}"),
    }
}

// ── Test 3: tracing ───────────────────────────────────────────────────────────

/// TracingHookAdapter wired as after_turn exports spans to InMemoryTraceExporter.
#[tokio::test]
async fn task_runner_tracing_emits_spans() {
    let exporter = Arc::new(InMemoryTraceExporter::new());
    let adapter = Arc::new(TracingHookAdapter::new(
        exporter.clone(),
        SpanCaptureConfig::default(),
    ));

    let mut hooks = HarnessHooks::none();
    hooks.after_turn = vec![adapter as Arc<dyn AfterTurnHook>];

    let runner = make_runner_with_hooks(vec![MockResponse::text("Hello from tracer.")], hooks);
    runner.start("trace me").await.unwrap();

    assert!(
        !exporter.exported_spans().is_empty(),
        "expected at least one span from after_turn hook"
    );
}
