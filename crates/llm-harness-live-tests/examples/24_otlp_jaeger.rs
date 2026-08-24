//! 24 — OTLP/Jaeger: Send spans to a real Jaeger collector via OTLP HTTP binary.
//!
//! Prerequisites:
//!   docker run -d -p 4318:4318 -p 16686:16686 \
//!     -e COLLECTOR_OTLP_ENABLED=true jaegertracing/all-in-one:latest
//!
//! Then:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 24_otlp_jaeger
//!
use llm_harness_platform::tracer::{CaptureLevel, SpanCaptureConfig, SpanStatus, TraceExporter};

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::HarnessHooks;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness_with, with_timeout,
};
use llm_harness_platform::tracing_adapter::TracingHookAdapter;
use llm_harness_trace_otel::{OtlpConfig, OtlpTraceExporter};
use llm_harness_types::{
    AfterProviderResponseHook, AfterToolCallHook, AfterTurnHook, BeforeProviderRequestHook,
    BeforeTurnHook, DataBlock, Tool, ToolContext, ToolFailure, ToolResult,
};

// ── Echo tool ────────────────────────────────────────────────────────────

struct EchoTool;

static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "Text to echo" }
        },
        "required": ["message"]
    })
});

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
        let msg = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("(no message)")
            .to_string();
        Box::pin(async move {
            Ok(ToolResult::full(
                vec![DataBlock::text(msg)],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

/// Build a `TracingHookAdapter` + `OtlpTraceExporter` pair.
fn build_otlp_tracing_hooks() -> (
    Arc<OtlpTraceExporter>,
    Arc<TracingHookAdapter>,
    HarnessHooks,
) {
    let exporter = Arc::new(
        OtlpTraceExporter::new(&OtlpConfig::default()).expect("failed to build OtlpTraceExporter"),
    );
    let adapter = Arc::new(TracingHookAdapter::new(
        exporter.clone(),
        SpanCaptureConfig {
            level: CaptureLevel::Full,
            redactor: None,
        },
    ));

    let hooks = HarnessHooks {
        before_turn: vec![adapter.clone() as Arc<dyn BeforeTurnHook>],
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
    println!("=== 24: OTLP/Jaeger ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .or(provider.anthropic.as_ref())
        .expect("set OPENAI_API_KEY or ANTHROPIC_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);
    println!("Make sure Jaeger is running:");
    println!("  docker run -d -p 4318:4318 -p 16686:16686 \\");
    println!("    -e COLLECTOR_OTLP_ENABLED=true jaegertracing/all-in-one:latest\n");
    println!("Traces will be visible at: http://localhost:16686\n");
    let (exporter, adapter, trace_hooks) = build_otlp_tracing_hooks();

    let (harness, _inspector, _tmp) =
        make_harness_with(entry, |b| b.hooks(trace_hooks).tool(Arc::new(EchoTool))).await;

    // Start a session span so all turn/llm_call/tool spans have a common parent.
    adapter.begin_session("otlp-example", &entry.model);

    let prompts = [
        "Use the echo tool to echo 'Hello OTLP 1'",
        "Use the echo tool to echo 'Hello OTLP 2'",
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
    // End the session span and flush remaining spans.
    adapter.end_session("otlp-example", SpanStatus::Ok);
    exporter.flush().await.ok();

    println!("\nDone! Check http://localhost:16686 for traces.");
    println!("Look for service: llm-harness");
}
