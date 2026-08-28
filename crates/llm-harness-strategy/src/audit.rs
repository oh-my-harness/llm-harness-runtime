//! Default audit logging hook.
//!
//! `ToolCallAuditHook` implements both `BeforeToolCallHook` and
//! `AfterToolCallHook`: the before hook records the start time, the after
//! hook constructs an `AuditEntry` (with duration) and writes it to an
//! `AuditSink`.
//!
//! `AuditPlugin` wraps the hook as a `Plugin` for one-line installation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use futures::future::BoxFuture;
use llm_harness_agent::{HarnessHooks, Plugin};
use llm_harness_types::audit::{AuditEntry, AuditEventType, AuditSink};
use llm_harness_types::{
    AfterToolCallCtx, AfterToolCallDecision, AfterToolCallHook, BeforeToolCallCtx,
    BeforeToolCallDecision, BeforeToolCallHook, EntryId,
};
use parking_lot::Mutex;

/// Hook that audits every tool call to an [`AuditSink`].
///
/// Implements both `BeforeToolCallHook` (records start time, returns `Allow`)
/// and `AfterToolCallHook` (constructs `AuditEntry` with duration, writes to
/// sink, returns `Passthrough`). Does not interfere with approval or result
/// modification.
///
/// # Security note
///
/// The full `args` of each tool call are recorded in the audit payload
/// without redaction. If tool arguments may contain secrets (API tokens,
/// passwords, etc.), ensure the `AuditSink` backend enforces appropriate
/// access controls, or pre-redact sensitive fields before they reach the hook.
pub struct ToolCallAuditHook {
    sink: Arc<dyn AuditSink>,
    trace_id: String,
    task_id: Option<String>,
    /// tool_use_id → start Instant, for duration calculation.
    start_times: Mutex<HashMap<String, Instant>>,
}

impl ToolCallAuditHook {
    /// Create a new audit hook writing to the given sink.
    pub fn new(sink: Arc<dyn AuditSink>) -> Self {
        Self {
            sink,
            trace_id: String::new(),
            task_id: None,
            start_times: Mutex::new(HashMap::new()),
        }
    }

    /// Set the trace ID for all audit entries produced by this hook.
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = trace_id.into();
        self
    }

    /// Set the task ID for all audit entries produced by this hook.
    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }
}

impl BeforeToolCallHook for ToolCallAuditHook {
    fn on_call<'a>(&'a self, ctx: BeforeToolCallCtx<'a>) -> BoxFuture<'a, BeforeToolCallDecision> {
        Box::pin(async move {
            self.start_times
                .lock()
                .insert(ctx.tool_use_id.to_string(), Instant::now());
            BeforeToolCallDecision::Allow
        })
    }
}

impl AfterToolCallHook for ToolCallAuditHook {
    fn on_complete<'a>(
        &'a self,
        ctx: AfterToolCallCtx<'a>,
    ) -> BoxFuture<'a, AfterToolCallDecision> {
        Box::pin(async move {
            let duration_ms = self
                .start_times
                .lock()
                .remove(ctx.tool_use_id)
                .map(|start| start.elapsed().as_millis() as u64);

            // ToolCall events record execution outcome in payload.result_status,
            // not in the decision field — AuditDecision is reserved for governance
            // decisions (approval, budget enforcement), not execution results.
            let payload = serde_json::json!({
                "tool_name": ctx.tool_name,
                "tool_use_id": ctx.tool_use_id,
                "args": ctx.args,
                "turn_index": ctx.turn_index,
                "result_status": if ctx.result.is_ok() { "success" } else { "failure" },
            });

            let entry = AuditEntry {
                timestamp: Utc::now(),
                trace_id: self.trace_id.clone(),
                session_id: EntryId(ctx.run.id().0),
                task_id: self.task_id.clone(),
                principal: "agent".into(),
                event_type: AuditEventType::ToolCall,
                payload,
                decision: None,
                cost_delta: None,
                duration_ms,
                prev_hash: None,
                hash: String::new(),
            };

            if let Err(e) = self.sink.record(entry).await {
                tracing::warn!("audit sink write failed: {e}");
            }

            AfterToolCallDecision::Passthrough
        })
    }
}

/// Plugin that installs [`ToolCallAuditHook`] for automatic tool call auditing.
///
/// # Example
///
/// ```ignore
/// use llm_harness_strategy::AuditPlugin;
/// use llm_harness_audit_jsonl::JsonlAuditSink;
/// use std::sync::Arc;
///
/// let sink = Arc::new(JsonlAuditSink::new("audit.jsonl"));
/// let harness = HarnessBuilder::new("claude-3-5-sonnet")
///     .install(&AuditPlugin::new(sink).with_trace_id("trace-123"))
///     .build(env)
///     .await?;
/// ```
pub struct AuditPlugin {
    sink: Arc<dyn AuditSink>,
    trace_id: String,
    task_id: Option<String>,
}

impl AuditPlugin {
    /// Create a new audit plugin writing to the given sink.
    pub fn new(sink: Arc<dyn AuditSink>) -> Self {
        Self {
            sink,
            trace_id: String::new(),
            task_id: None,
        }
    }

    /// Set the trace ID for all audit entries.
    pub fn with_trace_id(self, trace_id: impl Into<String>) -> Self {
        Self {
            trace_id: trace_id.into(),
            ..self
        }
    }

    /// Set the task ID for all audit entries.
    pub fn with_task_id(self, task_id: impl Into<String>) -> Self {
        Self {
            task_id: Some(task_id.into()),
            ..self
        }
    }
}

impl Plugin for AuditPlugin {
    fn name(&self) -> &str {
        "audit-defaults"
    }

    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        let hook = Arc::new(ToolCallAuditHook {
            sink: self.sink.clone(),
            trace_id: self.trace_id.clone(),
            task_id: self.task_id.clone(),
            start_times: Mutex::new(HashMap::new()),
        });
        hooks.before_tool_call.push(hook.clone());
        hooks.after_tool_call.push(hook);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{RunContext, RunRequest, ToolFailure, ToolResult};

    /// Mock sink that captures all entries.
    struct MockSink {
        entries: Mutex<Vec<AuditEntry>>,
    }

    impl MockSink {
        fn new() -> Self {
            Self {
                entries: Mutex::new(Vec::new()),
            }
        }

        fn entries(&self) -> Vec<AuditEntry> {
            self.entries.lock().clone()
        }
    }

    impl AuditSink for MockSink {
        fn record(&self, entry: AuditEntry) -> BoxFuture<'_, anyhow::Result<()>> {
            self.entries.lock().push(entry);
            Box::pin(async { Ok(()) })
        }

        fn flush(&self) -> BoxFuture<'_, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// Mock sink that always fails.
    struct FailingSink;

    impl AuditSink for FailingSink {
        fn record(&self, _entry: AuditEntry) -> BoxFuture<'_, anyhow::Result<()>> {
            Box::pin(async { Err(anyhow::anyhow!("sink failure")) })
        }

        fn flush(&self) -> BoxFuture<'_, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn make_run_context() -> RunContext {
        RunContext::new(RunRequest::from_text("test"))
    }

    fn make_assistant() -> llm_harness_types::AssistantMessage {
        llm_harness_loop::test_utils::test_assistant_message(vec![])
    }

    fn make_before_ctx<'a>(
        run: &'a RunContext,
        assistant: &'a llm_harness_types::AssistantMessage,
        tool_use_id: &'a str,
        tool_name: &'a str,
        args: &'a serde_json::Value,
    ) -> BeforeToolCallCtx<'a> {
        BeforeToolCallCtx {
            run,
            assistant_message: assistant,
            tool_use_id,
            tool_name,
            args,
            turn_index: 0,
        }
    }

    fn make_after_ctx<'a>(
        run: &'a RunContext,
        assistant: &'a llm_harness_types::AssistantMessage,
        tool_use_id: &'a str,
        tool_name: &'a str,
        args: &'a serde_json::Value,
        result: &'a Result<ToolResult, ToolFailure>,
    ) -> AfterToolCallCtx<'a> {
        AfterToolCallCtx {
            run,
            assistant_message: assistant,
            tool_use_id,
            tool_name,
            args,
            result,
            duration: std::time::Duration::from_millis(42),
            turn_index: 0,
        }
    }

    #[tokio::test]
    async fn before_after_writes_audit_entry() {
        let sink = Arc::new(MockSink::new());
        let hook = ToolCallAuditHook::new(sink.clone());

        let run = make_run_context();
        let assistant = make_assistant();
        let args = serde_json::json!({"path": "/tmp/test"});
        let tool_result = ToolResult::full(vec![], serde_json::json!({}), false);
        let result = Ok(tool_result);

        // Before
        let before_ctx = make_before_ctx(&run, &assistant, "tu-1", "read_file", &args);
        let decision = hook.on_call(before_ctx).await;
        assert!(matches!(decision, BeforeToolCallDecision::Allow));

        // After
        let after_ctx = make_after_ctx(&run, &assistant, "tu-1", "read_file", &args, &result);
        let after_decision = hook.on_complete(after_ctx).await;
        assert!(matches!(after_decision, AfterToolCallDecision::Passthrough));

        // Verify entry
        let entries = sink.entries();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.event_type, AuditEventType::ToolCall);
        assert_eq!(entry.principal, "agent");
        assert_eq!(entry.trace_id, "");
        assert!(entry.task_id.is_none());
        assert!(entry.cost_delta.is_none());
        assert!(entry.duration_ms.is_some());
        assert_eq!(entry.payload["tool_name"].as_str().unwrap(), "read_file");
        assert_eq!(entry.payload["tool_use_id"].as_str().unwrap(), "tu-1");
        assert_eq!(entry.payload["turn_index"].as_u64().unwrap(), 0);
        assert_eq!(entry.payload["result_status"].as_str().unwrap(), "success");
        assert!(entry.decision.is_none());
    }

    #[tokio::test]
    async fn duration_ms_is_nonzero_after_delay() {
        let sink = Arc::new(MockSink::new());
        let hook = ToolCallAuditHook::new(sink.clone());

        let run = make_run_context();
        let assistant = make_assistant();
        let args = serde_json::json!({});
        let tool_result = ToolResult::full(vec![], serde_json::json!({}), false);
        let result = Ok(tool_result);

        let before_ctx = make_before_ctx(&run, &assistant, "tu-2", "bash", &args);
        hook.on_call(before_ctx).await;

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let after_ctx = make_after_ctx(&run, &assistant, "tu-2", "bash", &args, &result);
        hook.on_complete(after_ctx).await;

        let entries = sink.entries();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].duration_ms.unwrap() >= 10);
    }

    #[tokio::test]
    async fn failure_result_recorded_in_payload() {
        let sink = Arc::new(MockSink::new());
        let hook = ToolCallAuditHook::new(sink.clone());

        let run = make_run_context();
        let assistant = make_assistant();
        let args = serde_json::json!({"path": "/nonexistent"});
        let failure = ToolFailure::new("not_found", "file not found");
        let result = Err(failure);

        let before_ctx = make_before_ctx(&run, &assistant, "tu-3", "read_file", &args);
        hook.on_call(before_ctx).await;

        let after_ctx = make_after_ctx(&run, &assistant, "tu-3", "read_file", &args, &result);
        hook.on_complete(after_ctx).await;

        let entries = sink.entries();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].decision.is_none());
        assert_eq!(
            entries[0].payload["result_status"].as_str().unwrap(),
            "failure"
        );
    }

    #[tokio::test]
    async fn sink_failure_does_not_block() {
        let sink = Arc::new(FailingSink);
        let hook = ToolCallAuditHook::new(sink);

        let run = make_run_context();
        let assistant = make_assistant();
        let args = serde_json::json!({});
        let tool_result = ToolResult::full(vec![], serde_json::json!({}), false);
        let result = Ok(tool_result);

        let before_ctx = make_before_ctx(&run, &assistant, "tu-4", "bash", &args);
        hook.on_call(before_ctx).await;

        let after_ctx = make_after_ctx(&run, &assistant, "tu-4", "bash", &args, &result);
        let decision = hook.on_complete(after_ctx).await;

        // Should still return Passthrough, not panic
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
    }

    #[tokio::test]
    async fn trace_id_and_task_id_propagated() {
        let sink = Arc::new(MockSink::new());
        let hook = ToolCallAuditHook::new(sink.clone())
            .with_trace_id("trace-abc")
            .with_task_id("task-xyz");

        let run = make_run_context();
        let assistant = make_assistant();
        let args = serde_json::json!({});
        let tool_result = ToolResult::full(vec![], serde_json::json!({}), false);
        let result = Ok(tool_result);

        let before_ctx = make_before_ctx(&run, &assistant, "tu-5", "bash", &args);
        hook.on_call(before_ctx).await;

        let after_ctx = make_after_ctx(&run, &assistant, "tu-5", "bash", &args, &result);
        hook.on_complete(after_ctx).await;

        let entries = sink.entries();
        assert_eq!(entries[0].trace_id, "trace-abc");
        assert_eq!(entries[0].task_id.as_ref().unwrap(), "task-xyz");
    }
}
