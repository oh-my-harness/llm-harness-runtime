//! 步骤间上下文注入的纯函数：step_history 摘要 + context vars 格式化。

use serde_json::Value;

use crate::workflow::model::{StepRecord, WorkflowContext};

/// 格式化 step_history 摘要，供步骤间上下文注入。
pub(crate) const STEP_HISTORY_OUTPUT_PREVIEW_CHARS: usize = 1_000;
pub(crate) const STEP_HISTORY_STRUCTURED_PREVIEW_CHARS: usize = 4_000;
/// 整个 history 注入的总字符上限。超出后按"丢弃最老"策略裁剪：
/// 保留最近的 records 直到总长度落入预算，被丢弃的早期 records
/// 以 `(earlier N steps omitted)` 提示 LLM 信息缺失。
/// 对齐 runtime issue #68 修复 1（无总预算 → 加总预算）。
pub(crate) const STEP_HISTORY_TOTAL_BUDGET_CHARS: usize = 20_000;

/// 截断到 `max_chars` 字符。若发生截断，追加 `(truncated N chars)` 元信息，
/// 让 LLM 知道尾部有内容被丢弃。对齐 runtime issue #68 修复 3。
pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    let rest: String = chars.collect();
    if !rest.is_empty() {
        format!("{preview}... (truncated {} chars)", rest.chars().count())
    } else {
        preview
    }
}

pub(crate) fn structured_preview(value: &Value) -> String {
    let serialized = serde_json::to_string(value)
        .unwrap_or_else(|err| format!("\"<failed to serialize structured result: {err}>\""));
    truncate_chars(&serialized, STEP_HISTORY_STRUCTURED_PREVIEW_CHARS)
}

/// 用 serde 序列化 Transition，产出 `{"to":"next"}` / `"retry"` 等
/// 干净的 JSON 标签，替代 `{:?}` Debug 串。对齐 runtime issue #68 修复 2。
fn transition_label(transition: &crate::workflow::model::Transition) -> String {
    serde_json::to_string(transition).unwrap_or_else(|_| format!("{transition:?}"))
}

/// 格式化单条 StepRecord 为文本行。对齐 runtime issue #68 修复 2/4。
fn format_step_record(r: &StepRecord) -> String {
    let transition = transition_label(&r.transition);

    let Some(result) = r.result.as_ref() else {
        // (no result) 分支：补 started_at / ended_at 时序线索，
        // 对断点续接场景（#62）有用。对齐 runtime issue #68 修复 4。
        return format!(
            "- step '{}' \u{2192} {} ({} \u{2192} {})\n  output: (no result)",
            r.step_id,
            transition,
            r.started_at.format("%H:%M:%S"),
            r.ended_at.format("%H:%M:%S"),
        );
    };

    let mut lines = vec![format!("- step '{}' \u{2192} {}", r.step_id, transition)];
    if let Some(structured) = result.structured.as_ref() {
        lines.push(format!("  structured: {}", structured_preview(structured)));
    }
    lines.push(format!(
        "  output: {}",
        truncate_chars(&result.output, STEP_HISTORY_OUTPUT_PREVIEW_CHARS)
    ));
    lines.join("\n")
}

/// 格式化 step_history 摘要，供步骤间上下文注入。
///
/// 总预算上限 (`STEP_HISTORY_TOTAL_BUDGET_CHARS`)：当 history 拼接后总长度
/// 超出预算时，丢弃最老的 records（最近的 step 对当前决策更重要），
/// 并在摘要开头标注被丢弃的数量。对齐 runtime issue #68 修复 1。
pub(crate) fn format_step_history_summary(history: &[StepRecord]) -> String {
    if history.is_empty() {
        return String::new();
    }

    // 逐条格式化，从最近的开始向前累积。
    let formatted: Vec<String> = history.iter().map(format_step_record).collect();

    // 从最近的 record 开始累积，直到总长度超出预算。
    let mut kept: Vec<&str> = Vec::with_capacity(formatted.len());
    let mut total_len: usize = 0;
    for entry in formatted.iter().rev() {
        let entry_len = entry.len() + 1; // +1 for the '\n' separator
        if total_len + entry_len > STEP_HISTORY_TOTAL_BUDGET_CHARS && !kept.is_empty() {
            break;
        }
        total_len += entry_len;
        kept.push(entry);
    }
    kept.reverse();

    let omitted = formatted.len() - kept.len();
    let mut summary = kept.join("\n");
    if omitted > 0 {
        summary = format!("(earlier {omitted} steps omitted)\n{summary}");
    }
    summary
}

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
