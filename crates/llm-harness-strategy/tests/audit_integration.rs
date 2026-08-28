//! Integration test: ToolCallAuditHook + JsonlAuditSink end-to-end.

use std::sync::Arc;

use llm_harness_audit_jsonl::JsonlAuditSink;
use llm_harness_loop::test_utils::test_assistant_message;
use llm_harness_strategy::audit::ToolCallAuditHook;
use llm_harness_types::{
    AfterToolCallCtx, AfterToolCallHook, BeforeToolCallCtx, BeforeToolCallHook, RunContext,
    RunRequest, ToolResult,
};

#[tokio::test]
async fn audit_hook_writes_to_jsonl_with_valid_hash_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let sink = Arc::new(JsonlAuditSink::new(&path));

    let hook = ToolCallAuditHook::new(sink).with_trace_id("trace-e2e");

    let run = RunContext::new(RunRequest::from_text("test"));
    let assistant = test_assistant_message(vec![]);
    let args = serde_json::json!({"command": "ls"});

    let tool_result = ToolResult::full(vec![], serde_json::json!({"exit_code": 0}), false);
    let result = Ok(tool_result);

    // Simulate two tool calls
    for i in 0..2 {
        let tool_use_id = format!("tu-{i}");
        let tool_name = if i == 0 { "bash" } else { "read_file" };

        let before_ctx = BeforeToolCallCtx {
            run: &run,
            assistant_message: &assistant,
            tool_use_id: &tool_use_id,
            tool_name,
            args: &args,
            turn_index: i,
        };
        hook.on_call(before_ctx).await;

        let after_ctx = AfterToolCallCtx {
            run: &run,
            assistant_message: &assistant,
            tool_use_id: &tool_use_id,
            tool_name,
            args: &args,
            result: &result,
            duration: std::time::Duration::from_millis(42),
            turn_index: i,
        };
        hook.on_complete(after_ctx).await;
    }

    // Validate hash chain
    let count = JsonlAuditSink::validate(&path).await.unwrap();
    assert_eq!(count, 2);

    // Verify content
    let content = tokio::fs::read_to_string(&path).await.unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2);

    // Each line should be valid JSON with trace_id
    for line in &lines {
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(val["trace_id"].as_str().unwrap(), "trace-e2e");
        assert_eq!(val["event_type"].as_str().unwrap(), "ToolCall");
        assert_eq!(val["principal"].as_str().unwrap(), "agent");
        assert!(val["duration_ms"].as_u64().is_some());
    }
}

#[test]
fn audit_plugin_registers_hooks() {
    use futures::future::BoxFuture;
    use llm_harness_agent::Plugin;
    use llm_harness_strategy::AuditPlugin;
    use llm_harness_types::audit::{AuditEntry, AuditSink};

    struct NullSink;
    impl AuditSink for NullSink {
        fn record(&self, _entry: AuditEntry) -> BoxFuture<'_, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn flush(&self) -> BoxFuture<'_, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    let plugin = AuditPlugin::new(std::sync::Arc::new(NullSink));
    assert_eq!(plugin.name(), "audit-defaults");

    let mut hooks = llm_harness_agent::HarnessHooks::none();
    plugin.register_hooks(&mut hooks);
    assert_eq!(hooks.before_tool_call.len(), 1);
    assert_eq!(hooks.after_tool_call.len(), 1);
}
