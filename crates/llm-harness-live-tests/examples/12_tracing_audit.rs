//! 12 — Tracing & Audit: SpanEvent export + JSONL hash-chain audit log.
//!
//! Demonstrates:
//! - `TracingHookAdapter` wired into all 4 HarnessHooks slots (BeforeProviderRequest,
//!   AfterProviderResponse, AfterToolCall, AfterTurn) with an `InMemoryTraceExporter`.
//! - `AuditPlugin` + `JsonlAuditSink` writing tool-call audit entries to a JSONL
//!   file with a SHA-256 hash chain, validated after the run.
//! - Both subsystems coexisting in the same harness.
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 12_tracing_audit

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::HarnessHooks;
use llm_harness_audit_jsonl::JsonlAuditSink;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness_with, with_timeout,
};
use llm_harness_runtime::tracer::{CaptureLevel, SpanCaptureConfig, SpanKind, SpanStatus};
use llm_harness_runtime::tracing_adapter::TracingHookAdapter;
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

/// Build a `TracingHookAdapter` + `InMemoryTraceExporter` pair, returning the
/// exporter (to read spans later) and the `HarnessHooks` fragment to merge.
fn build_tracing_hooks() -> (
    Arc<InMemoryTraceExporter>,
    Arc<TracingHookAdapter>,
    HarnessHooks,
) {
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

    (exporter, adapter, hooks)
}

#[tokio::main]
async fn main() {
    println!("=== 12: Tracing & Audit ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .or(provider.anthropic.as_ref())
        .expect("set OPENAI_API_KEY or ANTHROPIC_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    // ════════════════════════════════════════════════════════════════════════
    // Part 1: Tracing — TracingHookAdapter + InMemoryTraceExporter
    // ════════════════════════════════════════════════════════════════════════
    println!("── Part 1: Tracing ──\n");

    let (exporter, _adapter, trace_hooks) = build_tracing_hooks();

    let (harness, _tmp) =
        make_harness_with(entry, |b| b.hooks(trace_hooks).tool(Arc::new(EchoTool))).await;

    let prompts = [
        "Use the echo tool to echo 'Tracing test 1'",
        "Use the echo tool to echo 'Tracing test 2'",
        "Use the echo tool to echo 'Tracing test 3'",
    ];

    for (i, p) in prompts.iter().enumerate() {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(SINGLE_TURN, harness.prompt(*p))
            .await
            .expect("prompt failed");
        cap.settle().await;
        println!(
            "Turn {}: tools={:?} | {}...",
            i + 1,
            cap.tool_calls(),
            cap.texts().chars().take(60).collect::<String>()
        );
    }

    // Spans are exported as they complete. Drain by reading the exporter.
    let spans = exporter.exported_spans();
    println!("\nTotal spans exported: {}", spans.len());

    // Categorize by span name
    let mut names: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut kinds: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for s in &spans {
        *names.entry(s.name.as_str()).or_default() += 1;
        let kind_str = match s.kind {
            SpanKind::Internal => "Internal",
            SpanKind::Client => "Client",
            SpanKind::Server => "Server",
            SpanKind::Producer => "Producer",
            SpanKind::Consumer => "Consumer",
        };
        *kinds.entry(kind_str).or_default() += 1;
    }
    println!("\nSpan names:");
    for (name, count) in &names {
        println!("  {name}: {count}");
    }
    println!("\nSpan kinds:");
    for (kind, count) in &kinds {
        println!("  {kind}: {count}");
    }

    // Show first 3 spans in detail
    println!("\nSample spans (first 3):");
    for s in spans.iter().take(3) {
        let status_str = match &s.status {
            SpanStatus::Ok => "Ok".to_string(),
            SpanStatus::Error(e) => format!("Error({e})"),
            SpanStatus::Unset => "Unset".to_string(),
        };
        println!(
            "  [{}] name={:?} kind={:?} status={} attrs={}",
            s.span_id.0,
            s.name,
            s.kind,
            status_str,
            s.attributes.len()
        );
    }

    // ════════════════════════════════════════════════════════════════════════
    // Part 2: Audit — AuditPlugin + JsonlAuditSink
    // ════════════════════════════════════════════════════════════════════════
    println!("\n── Part 2: Audit ──\n");

    let audit_dir = tempfile::tempdir().unwrap();
    let audit_path = audit_dir.path().join("audit.jsonl");
    let sink: Arc<dyn AuditSink> = Arc::new(JsonlAuditSink::new(audit_path.clone()));

    let (harness2, _tmp2) = make_harness_with(entry, |b| {
        b.install(&AuditPlugin::new(sink).with_trace_id("trace-audit-demo"))
            .tool(Arc::new(EchoTool))
    })
    .await;

    let audit_prompts = [
        "Use the echo tool to echo 'Audit test 1'",
        "Use the echo tool to echo 'Audit test 2'",
    ];

    for (i, p) in audit_prompts.iter().enumerate() {
        let cap = EventCapture::subscribe(&harness2);
        with_timeout(SINGLE_TURN, harness2.prompt(*p))
            .await
            .expect("prompt failed");
        cap.settle().await;
        println!(
            "Turn {}: tools={:?} | {}...",
            i + 1,
            cap.tool_calls(),
            cap.texts().chars().take(60).collect::<String>()
        );
    }

    // Validate the hash chain
    let valid_count = JsonlAuditSink::validate(&audit_path)
        .await
        .expect("hash chain validation failed");
    println!("\nHash chain validation: {valid_count} valid entries");

    // Read and print the audit log
    let content = tokio::fs::read_to_string(&audit_path)
        .await
        .expect("failed to read audit file");
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    println!("Audit log entries: {}", lines.len());
    println!("\nAudit entries:");
    for (i, line) in lines.iter().enumerate() {
        let val: serde_json::Value = serde_json::from_str(line).expect("invalid JSON in audit log");
        let tool_name = val["payload"]["tool_name"].as_str().unwrap_or("(unknown)");
        let duration = val["duration_ms"].as_u64().unwrap_or(0);
        let principal = val["principal"].as_str().unwrap_or("?");
        let trace_id = val["trace_id"].as_str().unwrap_or("?");
        let hash_short = val["hash"]
            .as_str()
            .unwrap_or("?")
            .chars()
            .take(16)
            .collect::<String>();
        println!(
            "  [{i}] tool={tool_name} principal={principal} duration={duration}ms trace={trace_id} hash={hash_short}..."
        );
    }

    // ════════════════════════════════════════════════════════════════════════
    // Part 3: Combined — tracing + audit in one harness
    // ════════════════════════════════════════════════════════════════════════
    println!("\n── Part 3: Combined (Tracing + Audit) ──\n");

    let (exporter3, _adapter3, trace_hooks3) = build_tracing_hooks();

    let audit_path3 = _tmp2.path().join("combined_audit.jsonl");
    let sink3: Arc<dyn AuditSink> = Arc::new(JsonlAuditSink::new(audit_path3.clone()));

    let (harness3, _tmp3) = make_harness_with(entry, |b| {
        b.hooks(trace_hooks3)
            .install(&AuditPlugin::new(sink3).with_trace_id("trace-combined-demo"))
            .tool(Arc::new(EchoTool))
    })
    .await;

    let cap = EventCapture::subscribe(&harness3);
    with_timeout(
        SINGLE_TURN,
        harness3.prompt("Use the echo tool to echo 'Combined tracing and audit'"),
    )
    .await
    .expect("prompt failed");
    cap.settle().await;
    println!(
        "Turn 1: tools={:?} | {}...",
        cap.tool_calls(),
        cap.texts().chars().take(60).collect::<String>()
    );

    // Tracing results
    let spans3 = exporter3.exported_spans();
    println!("\nTracing: {} spans exported", spans3.len());
    let span_names3: Vec<&str> = spans3.iter().map(|s| s.name.as_str()).collect();
    println!("Span names: {span_names3:?}");

    // Audit results
    let valid3 = JsonlAuditSink::validate(&audit_path3)
        .await
        .expect("hash chain validation failed");
    println!("Audit: {valid3} valid entries in hash chain");

    let content3 = tokio::fs::read_to_string(&audit_path3)
        .await
        .expect("failed to read combined audit file");
    let combined_lines: Vec<&str> = content3.lines().filter(|l| !l.is_empty()).collect();
    println!("Combined audit entries: {}", combined_lines.len());

    // ── Summary ───────────────────────────────────────────────────────────
    println!("\n── Summary ──");
    println!(
        "Part 1: TracingHookAdapter emitted {} spans across 3 turns",
        spans.len()
    );
    println!(
        "Part 2: AuditPlugin wrote {} entries, hash chain valid ({} entries)",
        lines.len(),
        valid_count
    );
    println!(
        "Part 3: Both coexist — {} spans + {} audit entries from one run",
        spans3.len(),
        combined_lines.len()
    );
}
