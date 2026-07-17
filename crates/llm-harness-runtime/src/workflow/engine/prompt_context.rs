//! 步骤间上下文注入的纯函数：step_history 摘要 + context vars 格式化。

use serde_json::Value;

use crate::workflow::model::{StepRecord, WorkflowContext};

/// 格式化 step_history 摘要，供步骤间上下文注入。
pub(crate) const STEP_HISTORY_OUTPUT_PREVIEW_CHARS: usize = 1_000;
pub(crate) const STEP_HISTORY_STRUCTURED_PREVIEW_CHARS: usize = 4_000;

pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

pub(crate) fn structured_preview(value: &Value) -> String {
    let serialized = serde_json::to_string(value)
        .unwrap_or_else(|err| format!("\"<failed to serialize structured result: {err}>\""));
    truncate_chars(&serialized, STEP_HISTORY_STRUCTURED_PREVIEW_CHARS)
}

pub(crate) fn format_step_history_summary(history: &[StepRecord]) -> String {
    history
        .iter()
        .map(|r| {
            let Some(result) = r.result.as_ref() else {
                return format!(
                    "- step '{}' → {:?}\n  output: (no result)",
                    r.step_id, r.transition
                );
            };

            let mut lines = vec![format!("- step '{}' → {:?}", r.step_id, r.transition)];
            if let Some(structured) = result.structured.as_ref() {
                lines.push(format!("  structured: {}", structured_preview(structured)));
            }
            lines.push(format!(
                "  output: {}",
                truncate_chars(&result.output, STEP_HISTORY_OUTPUT_PREVIEW_CHARS)
            ));
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 把共享 context 的 KV 格式化为 prompt 摘要。空 context 返回空字符串。
pub(crate) fn format_context_vars(context: &WorkflowContext) -> String {
    if context.variables.is_empty() {
        return String::new();
    }
    let mut entries: Vec<(&String, &Value)> = context.variables.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
        .iter()
        .map(|(k, v)| format!("  {k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}
