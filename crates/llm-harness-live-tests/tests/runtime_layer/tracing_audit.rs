#![cfg(feature = "live-llm")]

//! Tests: tracing span export, JSONL audit log with hash chain, and
//! trace_id propagation between TracingHookAdapter and AuditPlugin.
//!
//! Covers (spec §7):
//! - TracingHookAdapter span export via InMemoryTraceExporter
//! - Span status correctness (at least one Client span for LLM calls)
//! - ToolCallAuditHook writes JSONL entries
//! - SHA-256 hash chain integrity
//! - trace_id propagation from AuditPlugin to audit entries

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::HarnessHooks;
use llm_harness_audit_jsonl::JsonlAuditSink;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, MULTI_TURN, make_harness_with, skip_if_none, with_timeout,
};
use llm_harness_platform::tracer::{CaptureLevel, SpanCaptureConfig, SpanKind};
use llm_harness_platform::tracing_adapter::TracingHookAdapter;
use llm_harness_strategy::AuditPlugin;
use llm_harness_trace_otel::InMemoryTraceExporter;
use llm_harness_types::audit::AuditSink;
use llm_harness_types::{
    AfterProviderResponseHook, AfterToolCallHook, AfterTurnHook, BeforeProviderRequestHook,
    DataBlock, Tool, ToolContext, ToolFailure, ToolResult,
};

// ── Echo tool (generates auditable tool calls) ───────────────────────────

struct EchoTool;

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

static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "The message to echo back" }
        },
        "required": ["message"]
    })
});

/// Three prompts that invite echo-tool usage across 3 turns.
const ECHO_PROMPTS: &[&str] = &[
    "Use the echo tool to echo 'Tracing test 1'.",
    "Use the echo tool to echo 'Tracing test 2'.",
    "Use the echo tool to echo 'Tracing test 3'.",
];

/// Build a TracingHookAdapter + InMemoryTraceExporter pair, returning the
/// exporter (to read spans later) and the HarnessHooks fragment to merge.
fn build_tracing_hooks() -> (Arc<InMemoryTraceExporter>, HarnessHooks) {
    let exporter = Arc::new(InMemoryTraceExporter::new());
    let adapter = Arc::new(TracingHookAdapter::new(
        exporter.clone(),
        SpanCaptureConfig {
            level: CaptureLevel::Full,
            redactor: None,
        },
    ));

    let hooks = HarnessHooks {
        before_provider_request: vec![adapter.clone() as Arc<dyn BeforeProviderRequestHook>],
        after_provider_response: vec![adapter.clone() as Arc<dyn AfterProviderResponseHook>],
        after_tool_call: vec![adapter.clone() as Arc<dyn AfterToolCallHook>],
        after_turn: vec![adapter.clone() as Arc<dyn AfterTurnHook>],
        ..HarnessHooks::none()
    };

    (exporter, hooks)
}

// ── Tests ────────────────────────────────────────────────────────────────

/// TracingHookAdapter emits spans; at least one is a Client (LLM call) span.
#[tokio::test]
async fn tracing_emits_spans() {
    let provider = LiveProvider::from_env();
    let entry = provider.openai.as_ref().or(provider.anthropic.as_ref());
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let (exporter, trace_hooks) = build_tracing_hooks();

    let (harness, _tmp) =
        make_harness_with(entry, |b| b.hooks(trace_hooks).tool(Arc::new(EchoTool))).await;

    for prompt in ECHO_PROMPTS {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(MULTI_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt should succeed");
        cap.settle().await;
    }

    let spans = exporter.exported_spans();
    assert!(
        !spans.is_empty(),
        "TracingHookAdapter should export at least one span"
    );

    let has_client = spans.iter().any(|s| s.kind == SpanKind::Client);
    assert!(
        has_client,
        "at least one span should be Client kind (LLM call); got kinds: {:?}",
        spans.iter().map(|s| s.kind).collect::<Vec<_>>()
    );
}

/// AuditPlugin + JsonlAuditSink writes JSONL with a valid SHA-256 hash chain,
/// and entries contain tool_name.
#[tokio::test]
async fn audit_writes_jsonl_and_validates_hash_chain() {
    let provider = LiveProvider::from_env();
    let entry = provider.openai.as_ref().or(provider.anthropic.as_ref());
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let audit_dir = tempfile::tempdir().unwrap();
    let audit_path = audit_dir.path().join("audit.jsonl");
    let sink: Arc<dyn AuditSink> = Arc::new(JsonlAuditSink::new(audit_path.clone()));

    let (harness, _tmp) = make_harness_with(entry, |b| {
        b.install(&AuditPlugin::new(sink).with_trace_id("test-trace"))
            .tool(Arc::new(EchoTool))
    })
    .await;

    for prompt in ECHO_PROMPTS {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(MULTI_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt should succeed");
        cap.settle().await;
    }

    let valid_count = JsonlAuditSink::validate(&audit_path)
        .await
        .expect("hash chain validation should succeed");
    assert!(
        valid_count >= 1,
        "hash chain should have at least 1 valid entry (got {valid_count})"
    );

    let content = tokio::fs::read_to_string(&audit_path)
        .await
        .expect("audit file should be readable");
    let entries: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        !entries.is_empty(),
        "audit log should contain at least one JSONL entry"
    );

    let has_tool_name = entries.iter().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .map(|v| v["payload"]["tool_name"].is_string())
            .unwrap_or(false)
    });
    assert!(
        has_tool_name,
        "at least one audit entry should contain payload.tool_name"
    );
}

/// When TracingHookAdapter and AuditPlugin share a trace_id, audit entries
/// contain the trace_id and spans are collected.
#[tokio::test]
async fn trace_id_propagates_to_audit() {
    let provider = LiveProvider::from_env();
    let entry = provider.openai.as_ref().or(provider.anthropic.as_ref());
    skip_if_none!(entry);
    let entry = entry.unwrap();
    let trace_id = "trace-propagation-test";
    let (exporter, trace_hooks) = build_tracing_hooks();

    let audit_dir = tempfile::tempdir().unwrap();
    let audit_path = audit_dir.path().join("audit.jsonl");
    let sink: Arc<dyn AuditSink> = Arc::new(JsonlAuditSink::new(audit_path.clone()));

    let (harness, _tmp) = make_harness_with(entry, |b| {
        b.hooks(trace_hooks)
            .install(&AuditPlugin::new(sink).with_trace_id(trace_id))
            .tool(Arc::new(EchoTool))
    })
    .await;

    for prompt in ECHO_PROMPTS {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(MULTI_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt should succeed");
        cap.settle().await;
    }

    // Spans were collected.
    let spans = exporter.exported_spans();
    assert!(
        !spans.is_empty(),
        "TracingHookAdapter should export spans during the run"
    );

    // Audit entries contain the trace_id.
    let content = tokio::fs::read_to_string(&audit_path)
        .await
        .expect("audit file should be readable");
    let entries: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        !entries.is_empty(),
        "audit log should contain at least one JSONL entry"
    );

    let all_have_trace_id = entries.iter().all(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .map(|v| v["trace_id"].as_str() == Some(trace_id))
            .unwrap_or(false)
    });
    assert!(
        all_have_trace_id,
        "every audit entry should carry trace_id={trace_id}"
    );
}
