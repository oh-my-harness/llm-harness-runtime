//! Output truncation + persistence helper.
//!
//! When tool output exceeds a threshold (lines or bytes), the full output is
//! persisted to a temp file and a truncated version (head + tail + marker) is
//! returned for the LLM. The marker text tells the LLM where to find the
//! complete output via the `read` tool.

use llm_harness_types::{ToolContext, ToolError};
use std::path::PathBuf;

/// Defaults follow the tutorial (ch4 line 247-252): trigger at >200 lines or
/// >10000 bytes; keep first 50 and last 50 lines.
#[derive(Debug, Clone)]
pub struct TruncationConfig {
    /// Trigger truncation when line count strictly exceeds this.
    pub max_lines: usize,
    /// Trigger truncation when byte count strictly exceeds this.
    pub max_bytes: usize,
    /// Lines to retain from the head when truncating.
    pub head_lines: usize,
    /// Lines to retain from the tail when truncating.
    pub tail_lines: usize,
}

const DEFAULT_MAX_LINES: usize = 200;
const DEFAULT_MAX_BYTES: usize = 10_000;
const DEFAULT_HEAD_LINES: usize = 50;
const DEFAULT_TAIL_LINES: usize = 50;

impl Default for TruncationConfig {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
            head_lines: DEFAULT_HEAD_LINES,
            tail_lines: DEFAULT_TAIL_LINES,
        }
    }
}

/// Result of [`truncate_and_persist`].
#[derive(Debug, Clone)]
pub struct TruncationOutcome {
    /// Final text for the model (may contain truncation marker).
    pub output: String,
    /// Whether truncation was applied.
    pub truncated: bool,
}

/// Check whether truncation is needed, and if so, persist the full output to
/// a temp file and return a truncated version (head + marker + tail).
///
/// When the output does not exceed thresholds, it is returned unchanged with
/// `truncated == false`.
///
/// **Degradation:** if `create_temp_dir` or `write_file` fails, the original
/// output is returned unchanged with `truncated == false` — never discard
/// the result just because persistence failed.
///
/// **Return type:** The `Result<TruncationOutcome, ToolError>` wrapper is
/// reserved for future error cases (e.g. when degradation is not acceptable
/// and persistence failures must propagate). The current implementation
/// always degrades to `Ok`; callers' `unwrap_or_else` fallbacks are
/// intentionally retained as defensive code for that future evolution.
pub async fn truncate_and_persist(
    raw_output: &str,
    ctx: &ToolContext,
    config: &TruncationConfig,
) -> Result<TruncationOutcome, ToolError> {
    // Check byte threshold first (O(1)) to avoid lines().collect() on
    // short outputs that don't need truncation.
    let exceeds_bytes = raw_output.len() > config.max_bytes;
    if !exceeds_bytes && raw_output.lines().count() <= config.max_lines {
        return Ok(TruncationOutcome {
            output: raw_output.to_string(),
            truncated: false,
        });
    }

    let lines: Vec<&str> = raw_output.lines().collect();
    // If line count is too small for head+tail to be meaningful, don't truncate.
    if lines.len() <= config.head_lines + config.tail_lines {
        return Ok(TruncationOutcome {
            output: raw_output.to_string(),
            truncated: false,
        });
    }

    // Persist full output to temp file.
    let temp_dir = match ctx.env.create_temp_dir("harness_output").await {
        Ok(dir) => dir,
        Err(_) => {
            // Degradation: return full output unchanged.
            return Ok(TruncationOutcome {
                output: raw_output.to_string(),
                truncated: false,
            });
        }
    };
    let file_path: PathBuf = temp_dir.join("output.txt");
    let abort = ctx.abort.clone();
    if ctx
        .env
        .write_file(&file_path, raw_output.as_bytes(), abort)
        .await
        .is_err()
    {
        // Degradation: return full output unchanged.
        return Ok(TruncationOutcome {
            output: raw_output.to_string(),
            truncated: false,
        });
    }

    // Build truncated text: head + marker + tail.
    let head: Vec<&str> = lines.iter().take(config.head_lines).copied().collect();
    let tail: Vec<&str> = lines
        .iter()
        .skip(lines.len() - config.tail_lines)
        .copied()
        .collect();
    let omitted = lines.len() - config.head_lines - config.tail_lines;
    let marker = format!(
        "\n... [省略 {} 行，完整输出已保存至 `{}`，可用 read 工具读取（传入 path 参数）] ...\n",
        omitted,
        file_path.display()
    );

    let mut result = String::with_capacity(
        head.iter().map(|s| s.len() + 1).sum::<usize>()
            + marker.len()
            + tail.iter().map(|s| s.len() + 1).sum::<usize>(),
    );
    for line in &head {
        result.push_str(line);
        result.push('\n');
    }
    result.push_str(&marker);
    for line in &tail {
        result.push_str(line);
        result.push('\n');
    }

    Ok(TruncationOutcome {
        output: result,
        truncated: true,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::MockExecutionEnv;
    use chrono::Utc;
    use llm_harness_types::{
        AssistantMessage, AssistantMessageKind, ExecutionEnv, RunContext, RunRequest, ToolContext,
    };
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn make_ctx(env: Arc<MockExecutionEnv>) -> ToolContext {
        let (tx, _rx) = mpsc::channel(1);
        ToolContext {
            run: Arc::new(RunContext::new(RunRequest::default())),
            env,
            abort: CancellationToken::new(),
            tool_use_id: "tu_test".into(),
            turn_index: 0,
            assistant_message: Arc::new(AssistantMessage {
                kind: AssistantMessageKind::FinalAnswer,
                message_id: "test_msg".into(),
                turn_id: "test_turn".into(),
                content: vec![],
                stop_reason: None,
                timestamp: Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: None,
                error_message: None,
            }),
            update_tx: tx,
        }
    }

    #[tokio::test]
    async fn no_truncation_under_threshold() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        let input = "line\n".repeat(100); // 100 lines, 500 bytes
        let outcome = truncate_and_persist(&input, &ctx, &TruncationConfig::default())
            .await
            .expect("should succeed");
        assert!(!outcome.truncated);
        assert_eq!(outcome.output, input);
    }
    #[tokio::test]
    async fn truncates_when_lines_exceed_threshold() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env.clone());
        // 300 lines — exceeds max_lines (200).
        let input: String = (1..=300)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = truncate_and_persist(&input, &ctx, &TruncationConfig::default())
            .await
            .expect("should succeed");

        assert!(outcome.truncated);
        // Should contain first 50 lines.
        assert!(
            outcome.output.contains("line 1\n"),
            "missing head: {}",
            &outcome.output[..100]
        );
        assert!(outcome.output.contains("line 50"), "missing line 50");
        // Should contain last 50 lines.
        assert!(outcome.output.contains("line 300"), "missing tail line 300");
        assert!(
            outcome.output.contains("line 251"),
            "missing tail start line 251"
        );
        // Should NOT contain middle lines.
        assert!(!outcome.output.contains("line 100\n"), "middle line leaked");
        // Should contain marker with omitted count (300 - 50 - 50 = 200).
        assert!(
            outcome.output.contains("省略 200 行"),
            "missing marker: {}",
            outcome.output
        );
        // Should contain temp file path.
        assert!(outcome.output.contains("output.txt"), "missing file path");
        assert!(
            outcome.output.contains("read 工具读取"),
            "missing read guidance"
        );
    }

    #[tokio::test]
    async fn truncates_when_bytes_exceed_threshold() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        // 150 lines but each line is long → total bytes > 10000.
        let long_line = "x".repeat(100);
        let input: String = (0..150)
            .map(|_| long_line.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            input.len() > 10_000,
            "test input should exceed byte threshold"
        );

        let outcome = truncate_and_persist(&input, &ctx, &TruncationConfig::default())
            .await
            .expect("should succeed");

        assert!(outcome.truncated);
        assert!(outcome.output.contains("省略"), "missing marker");
    }

    #[tokio::test]
    async fn no_truncation_at_exact_threshold() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        // Exactly 200 lines — not strictly greater.
        let input: String = (1..=200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = truncate_and_persist(&input, &ctx, &TruncationConfig::default())
            .await
            .expect("should succeed");
        assert!(!outcome.truncated);
    }

    #[tokio::test]
    async fn no_truncation_when_lines_too_few() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        // 80 lines (< head+tail=100) but bytes exceed threshold.
        let long_line = "x".repeat(200);
        let input: String = (0..80)
            .map(|_| long_line.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            input.len() > 10_000,
            "test input should exceed byte threshold"
        );

        let outcome = truncate_and_persist(&input, &ctx, &TruncationConfig::default())
            .await
            .expect("should succeed");
        assert!(
            !outcome.truncated,
            "should not truncate when lines <= head+tail"
        );
    }

    #[tokio::test]
    async fn temp_file_contains_full_output() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env.clone());
        let input: String = (1..=300)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = truncate_and_persist(&input, &ctx, &TruncationConfig::default())
            .await
            .expect("should succeed");
        assert!(outcome.truncated);

        // Extract file path from marker text.
        let path_start = outcome.output.find("已保存至 `").unwrap() + "已保存至 `".len();
        let path_end = outcome.output[path_start..].find("`").unwrap() + path_start;
        let file_path = std::path::Path::new(&outcome.output[path_start..path_end]);

        // Read the temp file and verify it contains the full output.
        let content = env
            .read_text_file(file_path, CancellationToken::new())
            .await
            .expect("should read temp file");
        assert_eq!(content, input);
    }

    #[tokio::test]
    async fn empty_string_not_truncated() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        let outcome = truncate_and_persist("", &ctx, &TruncationConfig::default())
            .await
            .expect("should succeed");
        assert!(!outcome.truncated);
        assert_eq!(outcome.output, "");
    }

    #[tokio::test]
    async fn degrades_when_create_temp_dir_fails() {
        let env = Arc::new(MockExecutionEnv::new());
        env.set_fail_temp_dir(true);
        let ctx = make_ctx(env);
        let input: String = (1..=300)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = truncate_and_persist(&input, &ctx, &TruncationConfig::default())
            .await
            .expect("should succeed with degradation");
        assert!(!outcome.truncated, "must not truncate on temp-dir failure");
        assert_eq!(outcome.output, input, "must return full output unchanged");
    }

    #[tokio::test]
    async fn degrades_when_write_file_fails() {
        let env = Arc::new(MockExecutionEnv::new());
        env.set_fail_write(true);
        let ctx = make_ctx(env);
        let input: String = (1..=300)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = truncate_and_persist(&input, &ctx, &TruncationConfig::default())
            .await
            .expect("should succeed with degradation");
        assert!(!outcome.truncated, "must not truncate on write failure");
        assert_eq!(outcome.output, input, "must return full output unchanged");
    }
}
