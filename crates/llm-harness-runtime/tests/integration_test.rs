//! Infrastructure assembly smoke test.
//!
//! Verifies that all runtime crates compose without type errors and that the
//! key integration points (sandbox I/O, audit recording, tracing spans) work
//! end-to-end within a single test.

use std::sync::Arc;

use chrono::Utc;
use llm_harness_agent::HarnessHooks;
use llm_harness_runtime::observability::audit::{AuditDecision, AuditEntry, AuditEventType, AuditSink};
use llm_harness_runtime::platform::sandbox::{ResourceLimits, Sandbox, SandboxConfig};
use llm_harness_runtime::observability::tracer::{CaptureLevel, SpanCaptureConfig, SpanStatus};
use llm_harness_runtime::observability::tracing_adapter::TracingHookAdapter;
use llm_harness_runtime_audit_jsonl::JsonlAuditSink;
use llm_harness_runtime_sandbox_os::OsEnvSandbox;
use llm_harness_runtime_trace_otel::InMemoryTraceExporter;
use llm_harness_types::{
    AfterProviderResponseHook, AfterToolCallHook, AfterTurnHook, BeforeProviderRequestHook, EntryId,
};
use tokio_util::sync::CancellationToken;

/// Smoke test: assemble all runtime crates, exercise key integration points.
///
/// Steps:
///   1. Create and start an OsEnvSandbox.
///   2. Write and read a file through ExecutionEnv.
///   3. Record audit entries and validate the hash chain.
///   4. Wire TracingHookAdapter into HarnessHooks.
///   5. Shutdown sandbox.
#[tokio::test]
async fn infrastructure_assembly_smoke_test() {
    // 1. Sandbox setup.
    let work_dir = tempfile::tempdir().unwrap();
    let config = SandboxConfig {
        fs_allowlist: vec![work_dir.path().to_path_buf()],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(work_dir.path().to_path_buf()),
    };
    let sandbox = OsEnvSandbox::new(config);
    sandbox.start().await.unwrap();
    assert!(sandbox.is_running());

    // 2. File I/O through ExecutionEnv.
    let env = sandbox.env();
    let abort = CancellationToken::new();
    env.write_file(
        std::path::Path::new("hello.txt"),
        b"hello world",
        abort.clone(),
    )
    .await
    .unwrap();
    let text = env
        .read_text_file(std::path::Path::new("hello.txt"), abort)
        .await
        .unwrap();
    assert_eq!(text, "hello world");

    // 3. Audit: record one entry and validate the chain.
    let audit_dir = tempfile::tempdir().unwrap();
    let audit_path = audit_dir.path().join("smoke.jsonl");
    let audit_sink = JsonlAuditSink::new(&audit_path);
    audit_sink
        .record(AuditEntry {
            timestamp: Utc::now(),
            trace_id: "smoke-trace".into(),
            session_id: EntryId(uuid::Uuid::new_v4()),
            task_id: Some("smoke-task".into()),
            principal: "agent".into(),
            event_type: AuditEventType::StateTransition,
            payload: serde_json::json!({"from": "pending", "to": "running"}),
            decision: Some(AuditDecision::Allow),
            cost_delta: None,
            prev_hash: None,
            hash: String::new(),
        })
        .await
        .unwrap();
    audit_sink.flush().await.unwrap();
    let validated = JsonlAuditSink::validate(&audit_path).await.unwrap();
    assert_eq!(validated, 1);

    // 4. Tracing: wire TracingHookAdapter into HarnessHooks.
    let exporter = Arc::new(InMemoryTraceExporter::new());
    let adapter = Arc::new(TracingHookAdapter::new(
        exporter.clone(),
        SpanCaptureConfig {
            level: CaptureLevel::MetadataOnly,
            redactor: None,
        },
    ));
    adapter.begin_task("smoke-task");
    adapter.begin_session("smoke-session", "claude-sonnet");
    adapter.begin_turn(0);
    adapter.end_session("smoke-session", SpanStatus::Ok);
    adapter.end_task("smoke-task", SpanStatus::Ok);

    let hooks = HarnessHooks {
        before_provider_request: vec![adapter.clone() as Arc<dyn BeforeProviderRequestHook>],
        after_provider_response: vec![adapter.clone() as Arc<dyn AfterProviderResponseHook>],
        after_tool_call: vec![adapter.clone() as Arc<dyn AfterToolCallHook>],
        after_turn: vec![adapter.clone() as Arc<dyn AfterTurnHook>],
        ..HarnessHooks::none()
    };
    assert!(hooks.before_tool_call.is_empty());

    // 5. Shutdown sandbox.
    Box::new(sandbox).shutdown().await.unwrap();
}

/// Verify dev configuration baseline: OsEnvSandbox + empty hooks.
#[tokio::test]
async fn dev_configuration_smoke_test() {
    let tmp = tempfile::tempdir().unwrap();
    let config = SandboxConfig {
        fs_allowlist: vec![tmp.path().to_path_buf()],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(tmp.path().to_path_buf()),
    };
    let sandbox = OsEnvSandbox::new(config);
    sandbox.start().await.unwrap();

    // Dev: no hooks.
    let hooks = HarnessHooks::none();
    assert!(hooks.before_provider_request.is_empty());
    assert!(hooks.after_turn.is_empty());

    Box::new(sandbox).shutdown().await.unwrap();
}

/// Staging configuration: full-capture tracing + audit sink assembled together.
#[tokio::test]
async fn staging_configuration_smoke_test() {
    // Full capture for debugging.
    let exporter = Arc::new(InMemoryTraceExporter::new());
    let trace_config = SpanCaptureConfig {
        level: CaptureLevel::Full,
        redactor: None,
    };
    let adapter = Arc::new(TracingHookAdapter::new(exporter, trace_config));

    let audit_dir = tempfile::tempdir().unwrap();
    let audit_path = audit_dir.path().join("staging.jsonl");
    let _audit_sink = Arc::new(JsonlAuditSink::new(&audit_path));

    // No human approval in staging (automated CI).
    let hooks = HarnessHooks {
        before_provider_request: vec![adapter.clone() as Arc<dyn BeforeProviderRequestHook>],
        after_provider_response: vec![adapter.clone() as Arc<dyn AfterProviderResponseHook>],
        after_tool_call: vec![adapter.clone() as Arc<dyn AfterToolCallHook>],
        after_turn: vec![adapter.clone() as Arc<dyn AfterTurnHook>],
        ..HarnessHooks::none()
    };

    assert!(hooks.before_tool_call.is_empty());
    assert!(!hooks.before_provider_request.is_empty());
}
