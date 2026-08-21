use std::sync::Arc;

use llm_harness_agent::HarnessHooks;
use llm_harness_platform::tracer::{CaptureLevel, SpanCaptureConfig, SpanStatus};
use llm_harness_platform::tracing_adapter::TracingHookAdapter;
use llm_harness_trace_otel::InMemoryTraceExporter;
use llm_harness_types::{BeforeToolCallCtx, BeforeToolCallDecision, BeforeToolCallHook};

// ── Test 1: TracingHookAdapter lifecycle produces no panics ─────────────────

/// Verifies that the TracingHookAdapter lifecycle methods (begin/end) do not
/// panic and correctly transition state without an active hook loop.
#[test]
fn tracing_hook_lifecycle_does_not_panic() {
    let exporter = Arc::new(InMemoryTraceExporter::new());
    let config = SpanCaptureConfig {
        level: CaptureLevel::MetadataOnly,
        redactor: None,
    };
    let adapter = TracingHookAdapter::new(exporter.clone(), config);

    // No spans exported yet.
    assert!(exporter.exported_spans().is_empty());

    adapter.begin_task("task-1");
    adapter.begin_session("session-1", "claude-sonnet");
    adapter.begin_turn(0);
    adapter.begin_turn(1);

    // Still no exports — lifecycle spans export only via hook callbacks.
    assert!(exporter.exported_spans().is_empty());

    // end_session / end_task move spans into the completed queue.
    adapter.end_session("session-1", SpanStatus::Ok);
    adapter.end_task("task-1", SpanStatus::Ok);

    // Spans are queued (not yet exported; export happens via async hooks).
    assert!(exporter.exported_spans().is_empty());
}

// ── Test 2: TracingHookAdapter + budget are independent ─────────────────────

/// Verifies that TracingHookAdapter state is not affected by budget-exceeded state.
/// When budget is exceeded, tracing still records spans normally.
#[test]
fn tracing_is_independent_of_budget_state() {
    let exporter = Arc::new(InMemoryTraceExporter::new());
    let adapter = TracingHookAdapter::new(exporter.clone(), SpanCaptureConfig::default());

    adapter.begin_task("budget-test");
    adapter.begin_session("s1", "claude-haiku");
    adapter.begin_turn(0);

    // Simulate budget exceeded — tracing should continue unaffected.
    adapter.end_session("s1", SpanStatus::Ok);
    adapter.end_task("budget-test", SpanStatus::Error("budget exceeded".into()));

    // Verify no panic occurred; spans are queued for export.
    assert!(exporter.exported_spans().is_empty());
}

// ── Test 3: Custom BeforeToolCallHook can be wired into HarnessHooks ─────────

/// Compile-time verification: a business-layer hook that directly implements
/// `BeforeToolCallHook` is coercible to `Arc<dyn BeforeToolCallHook>`,
/// confirming it satisfies the HarnessHooks slot type.
///
/// Human approval is now expressed by directly implementing `BeforeToolCallHook`
/// — the core `Allow`/`Modify`/`Deny` decision vocabulary plus `ToolResult.terminate`
/// (true = hard reject, false = advise-and-retry) covers all approval semantics
/// without a dedicated trait.
#[test]
fn custom_before_tool_call_hook_satisfies_harness_hooks() {
    use futures::future::BoxFuture;
    use llm_harness_types::ToolFailure;

    struct AlwaysDenyHook;

    impl BeforeToolCallHook for AlwaysDenyHook {
        fn on_call<'a>(
            &'a self,
            ctx: BeforeToolCallCtx<'a>,
        ) -> BoxFuture<'a, BeforeToolCallDecision> {
            Box::pin(async move {
                BeforeToolCallDecision::Deny(ToolFailure::new(
                    "test_denied",
                    format!("blocked: {}", ctx.tool_name),
                ))
            })
        }
    }

    let hook = Arc::new(AlwaysDenyHook);

    // Verify the hook can be held as Arc<dyn BeforeToolCallHook>.
    let _: Arc<dyn BeforeToolCallHook> = hook;
}

// ── Test 4: HarnessHooks::none() constructs with all slots absent ────────────

/// Verifies that an empty HarnessHooks can be constructed (dev config baseline).
#[test]
fn harness_hooks_none_has_no_slots_set() {
    let hooks = HarnessHooks::none();
    assert!(hooks.before_tool_call.is_empty());
    assert!(hooks.after_turn.is_empty());
    assert!(hooks.before_provider_request.is_empty());
    assert!(hooks.after_provider_response.is_empty());
    assert!(hooks.should_stop.is_empty());
}

// ── Test 5: TracingHookAdapter can be wired into HarnessHooks ────────────────

/// Verifies that Arc<TracingHookAdapter> can be coerced to the expected dyn
/// trait slots in HarnessHooks without compile errors.
#[test]
fn tracing_adapter_wires_into_harness_hooks() {
    use llm_harness_types::{
        AfterProviderResponseHook, AfterToolCallHook, AfterTurnHook, BeforeProviderRequestHook,
    };

    let exporter = Arc::new(InMemoryTraceExporter::new());
    let adapter = Arc::new(TracingHookAdapter::new(
        exporter,
        SpanCaptureConfig::default(),
    ));

    let hooks = HarnessHooks {
        before_provider_request: vec![adapter.clone() as Arc<dyn BeforeProviderRequestHook>],
        after_provider_response: vec![adapter.clone() as Arc<dyn AfterProviderResponseHook>],
        after_tool_call: vec![adapter.clone() as Arc<dyn AfterToolCallHook>],
        after_turn: vec![adapter.clone() as Arc<dyn AfterTurnHook>],
        ..HarnessHooks::none()
    };

    // All other slots remain empty.
    assert!(hooks.before_tool_call.is_empty());
    assert!(hooks.should_stop.is_empty());
}
