use futures::future::BoxFuture;
use llm_harness_types::*;
use serde_json::Value;

/// Maximum lines returned by a single Read call.
pub const MAX_LINES: usize = 2000;
/// Maximum bytes returned by a single Read call.
pub const MAX_BYTES: usize = 100 * 1024;

/// Read file contents with optional line-range selection.
pub struct ReadTool {
    schema: Value,
}

impl ReadTool {
    pub fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to read (relative or absolute)"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "1-indexed line number to start reading from"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read"
                    }
                },
                "required": ["file_path"]
            }),
        }
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Output is truncated to 2000 lines or 100 KB. \
         Use offset/limit for large files."
    }

    fn parameters_schema(&self) -> &Value {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move { read_file(args, ctx).await })
    }
}

async fn read_file(args: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
    let path_str = args["file_path"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("file_path required".into()))?;

    let offset = args["offset"].as_u64().map(|v| v as usize);
    let limit = args["limit"].as_u64().map(|v| v as usize);

    let abs_path = resolve_path(path_str, ctx.env.working_dir());

    let content = ctx
        .env
        .read_text_file(&abs_path, ctx.abort.clone())
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;

    let text = format_text_output(&content, offset, limit);
    Ok(ToolResult {
        content: vec![ContentBlock::Text { text }],
        details: Value::Null,
        terminate: false,
    })
}

fn resolve_path(path: &str, cwd: &std::path::Path) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Slice lines by offset/limit and apply MAX_LINES / MAX_BYTES truncation.
pub fn format_text_output(content: &str, offset: Option<usize>, limit: Option<usize>) -> String {
    let all_lines: Vec<&str> = content.split('\n').collect();
    let total_lines = all_lines.len();

    let start = offset.map(|o| o.saturating_sub(1)).unwrap_or(0);
    if start >= total_lines {
        return format!(
            "[Offset {} is beyond end of file ({} lines total)]",
            offset.unwrap_or(1),
            total_lines
        );
    }

    let end = limit
        .map(|l| (start + l).min(total_lines))
        .unwrap_or(total_lines);
    let selected = &all_lines[start..end];

    let (lines, truncated_by_lines) = if selected.len() > MAX_LINES {
        (&selected[..MAX_LINES], true)
    } else {
        (selected, false)
    };

    let mut byte_count = 0;
    let mut byte_limit_at = None;
    for (i, line) in lines.iter().enumerate() {
        byte_count += line.len() + 1;
        if byte_count > MAX_BYTES {
            byte_limit_at = Some(i);
            break;
        }
    }

    let (final_lines, truncated) = if let Some(n) = byte_limit_at {
        (&lines[..n], true)
    } else {
        (lines, truncated_by_lines)
    };

    let output_end = start + final_lines.len();
    let mut text = final_lines.join("\n");

    if truncated {
        let next_offset = output_end + 1;
        let remaining = total_lines - output_end;
        text.push_str(&format!(
            "\n\n[{} more lines. Use offset={} to continue.]",
            remaining, next_offset
        ));
    } else if end < total_lines {
        let next_offset = end + 1;
        let remaining = total_lines - end;
        text.push_str(&format!(
            "\n\n[{} more lines. Use offset={} to continue.]",
            remaining, next_offset
        ));
    }

    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_loop::test_utils::NoOpEnv;
    use std::io::Write as IoWrite;
    use std::sync::Arc;
    use tempfile::NamedTempFile;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn make_ctx(env: Arc<dyn ExecutionEnv>) -> (ToolContext, mpsc::Receiver<ToolResult>) {
        let (tx, rx) = mpsc::channel(4);
        let ctx = ToolContext {
            env,
            abort: CancellationToken::new(),
            tool_use_id: "test-id".into(),
            turn_index: 0,
            assistant_message: Arc::new(AssistantMessage {
                content: vec![],
                stop_reason: None,
                timestamp: chrono::Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: None,
                error_message: None,
            }),
            update_tx: tx,
        };
        (ctx, rx)
    }

    #[test]
    fn format_returns_full_content_for_small_file() {
        let content = "line1\nline2\nline3";
        let out = format_text_output(content, None, None);
        assert_eq!(out, "line1\nline2\nline3");
    }

    #[test]
    fn format_applies_offset_1indexed() {
        let content = "a\nb\nc\nd";
        let out = format_text_output(content, Some(2), None);
        assert!(out.starts_with("b\nc\nd"), "got: {:?}", out);
    }

    #[test]
    fn format_applies_limit() {
        let content = "a\nb\nc\nd\ne";
        let out = format_text_output(content, None, Some(2));
        assert!(out.starts_with("a\nb"), "got: {:?}", out);
        assert!(
            out.contains("more lines"),
            "should have continuation notice"
        );
    }

    #[test]
    fn format_offset_beyond_eof_returns_error_message() {
        let content = "a\nb";
        let out = format_text_output(content, Some(99), None);
        assert!(out.contains("beyond end of file"), "got: {:?}", out);
    }

    #[test]
    fn format_truncates_at_max_lines() {
        let content = (0..MAX_LINES + 10)
            .map(|i| format!("line{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let out = format_text_output(&content, None, None);
        assert!(out.contains("more lines"), "should be truncated");
        let line_count = out.lines().count();
        assert!(
            line_count <= MAX_LINES + 3,
            "too many lines: {}",
            line_count
        );
    }

    #[tokio::test]
    async fn execute_reads_real_file() {
        use llm_harness_runtime_sandbox_os::OsEnv;

        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "hello from file").unwrap();
        let path = tmp.path().to_path_buf();

        let env = Arc::new(OsEnv::new(path.parent().unwrap().to_path_buf()));
        let (ctx, _rx) = make_ctx(env);

        let tool = ReadTool::new();
        let args = serde_json::json!({ "file_path": path.to_str().unwrap() });
        let result = tool.execute(args, &ctx).await.unwrap();

        let text = match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("hello from file"), "got: {:?}", text);
    }

    #[tokio::test]
    async fn execute_returns_error_for_missing_file() {
        use llm_harness_runtime_sandbox_os::OsEnv;

        let env = Arc::new(OsEnv::new(std::path::PathBuf::from("/tmp")));
        let (ctx, _rx) = make_ctx(env);

        let tool = ReadTool::new();
        let args = serde_json::json!({ "file_path": "/tmp/nonexistent_file_xyz_12345.txt" });
        let result = tool.execute(args, &ctx).await;
        assert!(result.is_err(), "expected error for missing file");
    }

    #[tokio::test]
    async fn execute_with_noop_env_returns_error() {
        let env = Arc::new(NoOpEnv);
        let (ctx, _rx) = make_ctx(env);

        let tool = ReadTool::new();
        let args = serde_json::json!({ "file_path": "/any/path" });
        let result = tool.execute(args, &ctx).await;
        assert!(result.is_err(), "NoOpEnv should fail");
    }
}
