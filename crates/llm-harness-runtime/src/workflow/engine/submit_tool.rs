//! 内置 tool：让 step agent 提交结构化结果（JSON）。

use std::sync::Arc;
use std::sync::LazyLock;

use futures::future::BoxFuture;
use llm_harness_types::{
    ContentBlock, SUBMIT_STEP_RESULT_TOOL_NAME, Tool, ToolContext, ToolError, ToolLevel, ToolResult,
};
use serde_json::Value;
use tokio::sync::Mutex;

/// SubmitStepResultTool 提交的 JSON payload 大小上限（bytes）。
pub(crate) const SUBMIT_RESULT_MAX_BYTES: usize = 256 * 1024;

/// 内置 tool：让 step agent 提交结构化结果（JSON）。
/// 引擎在 step 收尾时把提交的 JSON 填入 StepResult.structured，
/// judge 直接读 ctx.last_result.structured，不用自己经 TaskStore 反查 checkpoint。
pub(crate) struct SubmitStepResultTool {
    /// 提交的结果经 channel 传给引擎（run_step 在收集事件时读取）。
    tx: Arc<Mutex<Option<Value>>>,
}

impl SubmitStepResultTool {
    pub(crate) fn new() -> (Self, Arc<Mutex<Option<Value>>>) {
        let tx = Arc::new(Mutex::new(None));
        (Self { tx: tx.clone() }, tx)
    }
}

fn submit_step_result_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "result": { "type": "object", "description": "结构化结果（任意 JSON）" }
        },
        "required": ["result"]
    })
}

impl Tool for SubmitStepResultTool {
    fn name(&self) -> &str {
        SUBMIT_STEP_RESULT_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }
    fn description(&self) -> &str {
        "提交本步的结构化结果（JSON）并结束本步。judge 会直接读此结果做跳转决策。调用此 tool 后无需再输出文本总结。"
    }
    fn parameters_schema(&self) -> &'static serde_json::Value {
        static S: LazyLock<serde_json::Value> = LazyLock::new(submit_step_result_schema);
        &S
    }
    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolError>> {
        let tx = self.tx.clone();
        Box::pin(async move {
            let result = args.get("result").cloned().unwrap_or(Value::Null);
            // 结构校验：result 必须是 object（结构化结果）。
            if !result.is_object() {
                return Err(ToolError::Execution(
                    "submit_step_result: 'result' must be a JSON object".into(),
                ));
            }
            // 大小限制：防止 LLM 注入巨大 JSON 撑爆内存/落盘。
            let serialized = serde_json::to_string(&result)
                .map_err(|e| ToolError::Execution(format!("invalid JSON: {e}")))?;
            if serialized.len() > SUBMIT_RESULT_MAX_BYTES {
                return Err(ToolError::Execution(format!(
                    "submit_step_result: payload exceeds {} bytes",
                    SUBMIT_RESULT_MAX_BYTES
                )));
            }
            // last-write-wins：多次调用后者覆盖前者。
            *tx.lock().await = Some(result.clone());
            Ok(ToolResult {
                content: vec![ContentBlock::Text {
                    text: "Step result submitted.".into(),
                }],
                details: serde_json::json!({ "submitted": true }),
                terminate: true,
            })
        })
    }
}

llm_harness_types::assert_builtin_registered!(SubmitStepResultTool => SUBMIT_STEP_RESULT_TOOL_NAME);
