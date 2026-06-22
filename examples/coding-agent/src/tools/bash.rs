use futures::future::BoxFuture;
use llm_harness_types::*;
use serde_json::Value;

/// Maximum combined stdout+stderr bytes returned by a single Bash call.
pub const MAX_OUTPUT_BYTES: usize = 100 * 1024;

/// Bytes kept from the head of oversized output.
pub const HEAD_BYTES: usize = 40 * 1024;
/// Bytes kept from the tail of oversized output.
pub const TAIL_BYTES: usize = 40 * 1024;

/// Execute a shell command and return its combined stdout + stderr.
pub struct BashTool {
    schema: Value,
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Bash command to execute"
                    },
                    "timeout": {
                        "type": "number",
                        "description": "Timeout in seconds (optional)"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory for the command (optional)"
                    }
                },
                "required": ["command"]
            }),
        }
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command and return combined stdout and stderr. \
         Output is truncated to 100 KB."
    }

    fn parameters_schema(&self) -> &Value {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move { run_bash(args, ctx).await })
    }
}

async fn run_bash(args: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
    let command = args["command"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("command required".into()))?;

    let timeout_secs = args["timeout"].as_f64();
    let cwd = args["cwd"].as_str().map(std::path::Path::new);

    let opts = ShellOptions {
        cwd,
        env: vec![],
        timeout: timeout_secs.map(std::time::Duration::from_secs_f64),
        abort: ctx.abort.clone(),
        on_stdout: None,
        on_stderr: None,
    };

    let output = ctx.env.execute_shell(command, opts).await;
    let output = match output {
        Ok(o) => o,
        Err(EnvError::ShellFailed {
            exit_code,
            stdout,
            stderr,
        }) => ShellOutput {
            stdout,
            stderr,
            exit_code,
        },
        Err(e) => return Err(ToolError::Execution(e.to_string())),
    };

    let text = format_output(&output);
    Ok(ToolResult {
        content: vec![ContentBlock::Text { text }],
        details: serde_json::json!({ "exit_code": output.exit_code }),
        terminate: false,
    })
}

/// Combine stdout + stderr, truncate to MAX_OUTPUT_BYTES, annotate exit code.
///
/// When output exceeds MAX_OUTPUT_BYTES, keeps the first HEAD_BYTES and the
/// last TAIL_BYTES, inserting a "[... N bytes omitted ...]" marker in between.
/// This ensures both the beginning of output (context) and the end (errors,
/// test results) are always visible to the LLM.
pub fn format_output(output: &ShellOutput) -> String {
    let combined = format!("{}{}", output.stdout, output.stderr);
    let truncated = if combined.len() > MAX_OUTPUT_BYTES {
        let head_end = combined.floor_char_boundary(HEAD_BYTES);
        let tail_start = combined.ceil_char_boundary(combined.len() - TAIL_BYTES);
        let omitted = tail_start - head_end;
        format!(
            "{}\n[... {} bytes omitted ...]\n{}",
            &combined[..head_end],
            omitted,
            &combined[tail_start..]
        )
    } else {
        combined
    };

    if output.exit_code != 0 {
        format!("{}\n[Exit code: {}]", truncated, output.exit_code)
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_runtime_sandbox_os::OsEnv;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn make_ctx(env: Arc<dyn ExecutionEnv>) -> ToolContext {
        let (tx, _rx) = mpsc::channel(4);
        ToolContext {
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
        }
    }

    #[test]
    fn format_output_success() {
        let out = ShellOutput {
            stdout: "hello\n".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let text = format_output(&out);
        assert_eq!(text, "hello\n");
    }

    #[test]
    fn format_output_nonzero_exit_appends_code() {
        let out = ShellOutput {
            stdout: "oops\n".into(),
            stderr: String::new(),
            exit_code: 1,
        };
        let text = format_output(&out);
        assert!(text.contains("[Exit code: 1]"), "got: {:?}", text);
    }

    #[test]
    fn format_output_truncates_large_output() {
        let big = "x".repeat(MAX_OUTPUT_BYTES + 100);
        let out = ShellOutput {
            stdout: big,
            stderr: String::new(),
            exit_code: 0,
        };
        let text = format_output(&out);
        assert!(text.contains("omitted"), "got: {:?}", &text[..100]);
        // head + tail + marker overhead — well under 2 * MAX_OUTPUT_BYTES
        assert!(text.len() <= HEAD_BYTES + TAIL_BYTES + 200);
    }

    #[test]
    fn format_output_head_and_tail_both_preserved() {
        // Build output that clearly exceeds MAX_OUTPUT_BYTES.
        // Head: HEAD_BYTES of 'H', Middle: enough 'M' to exceed limit, Tail: TAIL_BYTES of 'T'.
        let head = "H".repeat(HEAD_BYTES);
        let middle = "M".repeat(MAX_OUTPUT_BYTES); // ensures total >> MAX_OUTPUT_BYTES
        let tail = "T".repeat(TAIL_BYTES);
        let big = format!("{}{}{}", head, middle, tail);

        let out = ShellOutput {
            stdout: big,
            stderr: String::new(),
            exit_code: 0,
        };
        let text = format_output(&out);

        // Head content is present
        assert!(text.starts_with(&"H".repeat(100)), "head missing");
        // Tail content is present
        assert!(text.ends_with(&"T".repeat(100)), "tail missing");
        // Omission marker is present
        assert!(text.contains("bytes omitted"), "omitted marker missing");
        // Middle content is not in the output
        assert!(!text.contains("MMM"), "middle should be omitted");
    }

    #[test]
    fn format_output_combines_stdout_stderr() {
        let out = ShellOutput {
            stdout: "out".into(),
            stderr: "err".into(),
            exit_code: 0,
        };
        let text = format_output(&out);
        assert!(
            text.contains("out") && text.contains("err"),
            "got: {:?}",
            text
        );
    }

    #[tokio::test]
    async fn execute_echo_command() {
        let env = Arc::new(OsEnv::new(std::path::PathBuf::from("/tmp")));
        let ctx = make_ctx(env);

        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "echo hello_world" });
        let result = tool.execute(args, &ctx).await.unwrap();

        let text = match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("hello_world"), "got: {:?}", text);
    }

    #[tokio::test]
    async fn execute_nonzero_exit_returns_ok_with_exit_code() {
        let env = Arc::new(OsEnv::new(std::path::PathBuf::from("/tmp")));
        let ctx = make_ctx(env);

        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "exit 42" });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(
            text.contains("42"),
            "should report exit code 42; got: {:?}",
            text
        );
    }

    #[tokio::test]
    async fn execute_captures_stderr() {
        let env = Arc::new(OsEnv::new(std::path::PathBuf::from("/tmp")));
        let ctx = make_ctx(env);

        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "echo err_output >&2" });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("err_output"), "got: {:?}", text);
    }
}
