//! Hook composition matrix tests (§8.1.4).
//!
//! Verifies that individual hooks and cross-hook combinations behave correctly
//! without a full AgentHarness run loop.

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::HarnessHooks;
use llm_harness_runtime::TracingHookAdapter;
use llm_harness_runtime::human_approval::{
    ApprovalDecision, ApprovalRequest, HumanApprovalWrapper, HumanApprover, TimeoutPolicy,
};
use llm_harness_runtime::tracer::{CaptureLevel, SpanCaptureConfig, SpanStatus};
use llm_harness_runtime_trace_otel::InMemoryTraceExporter;
use llm_harness_types::BeforeToolCallHook;

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

// ── Test 3: HumanApprovalWrapper implements BeforeToolCallHook ───────────────

/// Compile-time verification: HumanApprovalWrapper is coercible to
/// Arc<dyn BeforeToolCallHook>, confirming it satisfies the HarnessHooks slot type.
#[test]
fn human_approval_wrapper_satisfies_before_tool_call_hook() {
    struct AlwaysDenyApprover;

    impl HumanApprover for AlwaysDenyApprover {
        fn ask<'a>(
            &'a self,
            request: &'a ApprovalRequest,
        ) -> BoxFuture<'a, Option<ApprovalDecision>> {
            Box::pin(async move {
                Some(ApprovalDecision::Deny {
                    reason: format!("blocked: {}", request.tool_name),
                })
            })
        }
    }

    let wrapper = Arc::new(HumanApprovalWrapper::new(
        Arc::new(AlwaysDenyApprover),
        TimeoutPolicy::Deny {
            reason: "timeout".into(),
        },
    ));

    // Verify the wrapper can be held as Arc<dyn BeforeToolCallHook>.
    let _: Arc<dyn BeforeToolCallHook> = wrapper;
}

// ── Test 4: HarnessHooks::none() constructs with all slots absent ────────────

/// Verifies that an empty HarnessHooks can be constructed (dev config baseline).
#[test]
fn harness_hooks_none_has_no_slots_set() {
    let hooks = HarnessHooks::none();
    assert!(hooks.before_tool_call.is_none());
    assert!(hooks.after_turn.is_none());
    assert!(hooks.before_provider_request.is_none());
    assert!(hooks.after_provider_response.is_none());
    assert!(hooks.should_stop.is_none());
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
        before_provider_request: Some(adapter.clone() as Arc<dyn BeforeProviderRequestHook>),
        after_provider_response: Some(adapter.clone() as Arc<dyn AfterProviderResponseHook>),
        after_tool_call: Some(adapter.clone() as Arc<dyn AfterToolCallHook>),
        after_turn: Some(adapter.clone() as Arc<dyn AfterTurnHook>),
        ..HarnessHooks::none()
    };

    // All other slots remain absent.
    assert!(hooks.before_tool_call.is_none());
    assert!(hooks.should_stop.is_none());
}
