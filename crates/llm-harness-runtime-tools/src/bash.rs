//! The `bash` tool: execute a shell command via [`ExecutionEnv::execute_shell`]
//! and surface the merged stdout/stderr plus exit code back to the LLM.

use std::time::Duration;

use futures::future::BoxFuture;
use llm_harness_types::{
    DataBlock, EnvError, ShellOptions, ShellOutput, Tool, ToolContext, ToolError, ToolFailure,
    ToolResult,
};

use crate::resolve_path;
use crate::truncation::{TruncationConfig, TruncationOutcome, truncate_and_persist};

/// Default timeout (seconds) when `timeout` is not supplied.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Minimum allowed timeout (seconds). Values below this are clamped up.
const MIN_TIMEOUT_SECS: u64 = 1;

/// Maximum allowed timeout (seconds). Values above this are clamped down.
const MAX_TIMEOUT_SECS: u64 = 3600;

/// `bash` tool — run a shell command and return its output.
///
/// The command runs through [`ExecutionEnv::execute_shell`]; stdout and stderr
/// are merged (stdout first) into the returned text. An exit code of 0 yields a
/// normal result; a non-zero exit code is conveyed via a trailing
/// `Command exited with code <n>` line and a `details.exit_code` field rather
/// than as a `ToolFailure` (the command *ran*, it just failed).
pub struct BashTool;

impl BashTool {
    /// Create a new `BashTool`. The tool holds no state.
    pub fn new() -> Self {
        Self
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
        "Execute a shell command and return its merged stdout/stderr output.\n\n\
         Use when: running CLI tools, scripts, or git commands; checking build/test results; inspecting system state.\n\
         Don't use for: reading file contents (use read); writing files (use write); editing files (use edit); searching file contents (use grep); finding files by pattern (use glob); long-running daemons or interactive sessions.\n\
         Returns: Merged stdout+stderr text. Long output is truncated (head+tail shown, full output saved to a temp file path included in the result).\n\n\
         Examples:\n\
         {\"command\": \"git status\"}\n\
         {\"command\": \"ls -la\", \"cwd\": \"src/\"}\n\
         {\"command\": \"cargo test\", \"timeout\": 600}\n\
         {\"command\": \"RUST_LOG=debug cargo run\", \"env\": {\"RUST_LOG\": \"debug\"}}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute. e.g. \"git status\", \"ls -la\", \"cargo test\""
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory (relative or absolute). e.g. \"src/\" or \"/tmp\""
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds (1..3600, default 300). e.g. 120",
                        "minimum": MIN_TIMEOUT_SECS,
                        "maximum": MAX_TIMEOUT_SECS,
                        "default": DEFAULT_TIMEOUT_SECS
                    },
                    "env": {
                        "type": "object",
                        "description": "Additional environment variables. e.g. {\"RUST_LOG\": \"debug\", \"NODE_ENV\": \"test\"}",
                        "additionalProperties": { "type": "string" }
                    }
                },
                "required": ["command"]
            })
        });
        &SCHEMA
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let command = args
                .get("command")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ToolError::InvalidArguments("missing string field 'command'".into())
                })?;

            // Resolve cwd if provided; relative paths are resolved against the
            // env working dir. The `PathBuf` owns the storage for the borrow.
            let cwd_buf = args
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .map(|s| resolve_path(ctx.env.working_dir(), s));
            let cwd = cwd_buf.as_deref();

            // Clamp timeout into the valid range.
            let timeout = args
                .get("timeout")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS);

            // Collect env entries. The string keys/values must outlive the
            // `ShellOptions`, so we own them in a `Vec<(String, String)>` and
            // borrow into a parallel `Vec<(&str, &str)>`.
            let env_owned: Vec<(String, String)> = args
                .get("env")
                .and_then(serde_json::Value::as_object)
                .map(|map| {
                    map.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let env_vec: Vec<(&str, &str)> = env_owned
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();

            let opts = ShellOptions {
                cwd,
                env: env_vec,
                timeout: Some(Duration::from_secs(timeout)),
                abort: ctx.abort.clone(),
                on_stdout: None,
                on_stderr: None,
            };

            let output = match ctx.env.execute_shell(command, opts).await {
                Ok(o) => o,
                Err(EnvError::Aborted) => return Err(ToolFailure::aborted()),
                // The real `OsEnv` signals a command timeout as
                // `EnvError::ShellFailed { exit_code: -1, stderr: "timeout" }`
                // (env.rs:458-464; asserted by `execute_shell_timeout_returns_shell_failed`).
                // Per spec (design.md line 157) a timeout must surface as a
                // `ToolFailure` carrying no captured output — not as a normal
                // `ToolResult`. We detect the timeout via the `stderr ==
                // "timeout"` sentinel (OsEnv's documented contract) and route
                // it to a sanitized execution failure.
                Err(EnvError::ShellFailed {
                    exit_code: _,
                    stdout,
                    stderr,
                }) if stderr == "timeout" => {
                    let mut msg = String::from("command timed out");
                    if !stdout.is_empty() {
                        msg.push_str("; stdout: ");
                        msg.push_str(&stdout);
                    }
                    if !stderr.is_empty() {
                        msg.push_str("; stderr: ");
                        msg.push_str(&stderr);
                    }
                    return Err(ToolError::Execution(msg).into());
                }
                // Non-zero exit: the real OsEnv returns `EnvError::ShellFailed`
                // carrying the captured stdout/stderr. Convert it to a
                // `ToolResult` (not `ToolFailure`) so the LLM can see the command
                // output and adjust — matching the spec's "ran but failed"
                // semantics.
                Err(EnvError::ShellFailed {
                    exit_code,
                    stdout,
                    stderr,
                }) => ShellOutput {
                    stdout,
                    stderr,
                    exit_code,
                },
                Err(e) => return Err(ToolError::Execution(e.to_string()).into()),
            };

            // Merge stdout then stderr.
            let mut text = output.stdout;
            if !output.stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&output.stderr);
            }

            // Truncate + persist if output exceeds thresholds.
            let outcome = truncate_and_persist(&text, ctx, &TruncationConfig::default())
                .await
                .unwrap_or_else(|_| TruncationOutcome {
                    output: text.clone(),
                    truncated: false,
                });
            let text = outcome.output;

            if output.exit_code == 0 {
                let text = if text.is_empty() {
                    "(no output)".to_string()
                } else {
                    text
                };
                let details = serde_json::json!({
                    "exit_code": output.exit_code,
                    "truncated": outcome.truncated,
                });
                Ok(ToolResult::full(
                    vec![DataBlock::text(text)],
                    details,
                    false,
                ))
            } else {
                // Non-zero exit: the command ran but failed. Surface the exit
                // code both in the text and in `details`. The exit-code line
                // is appended *after* truncation — it is metadata, not command
                // output, and must not be truncated.
                let mut text = text;
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&format!("Command exited with code {}", output.exit_code));
                let details = serde_json::json!({
                    "exit_code": output.exit_code,
                    "truncated": outcome.truncated,
                });
                Ok(ToolResult::full(
                    vec![DataBlock::text(text)],
                    details,
                    false,
                ))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::MockExecutionEnv;
    use chrono::Utc;
    use std::sync::Arc;

    use llm_harness_types::{AssistantMessage, AssistantMessageKind};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Build a `ToolContext` backed by `env` with a no-op update channel.
    fn make_ctx(env: Arc<MockExecutionEnv>) -> ToolContext {
        let (tx, _rx) = mpsc::channel(1);
        ToolContext {
            run: Arc::new(llm_harness_types::RunContext::new(
                llm_harness_types::RunRequest::default(),
            )),
            env,
            abort: CancellationToken::new(),
            tool_use_id: "tu_bash".into(),
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

    fn text_of(res: &ToolResult) -> String {
        match res.model_content.as_slice() {
            [DataBlock::Text { text, .. }] => text.clone(),
            _ => panic!("expected a single text block, got {:?}", res.model_content),
        }
    }

    #[tokio::test]
    async fn bash_echo() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        let tool = BashTool::new();

        let res = tool
            .execute(serde_json::json!({ "command": "echo hello" }), &ctx)
            .await
            .expect("echo should succeed");

        assert_eq!(text_of(&res), "hello");
        assert_eq!(res.details["exit_code"], 0);
    }
    #[tokio::test]
    async fn bash_short_output_not_truncated() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        let tool = BashTool::new();

        let res = tool
            .execute(serde_json::json!({ "command": "echo hello" }), &ctx)
            .await
            .expect("echo should succeed");

        assert_eq!(text_of(&res), "hello");
        assert_eq!(res.details["truncated"], false);
    }

    #[tokio::test]
    async fn bash_nonzero_exit() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        let tool = BashTool::new();

        let res = tool
            .execute(serde_json::json!({ "command": "false" }), &ctx)
            .await
            .expect("false should return a result, not an error");

        let text = text_of(&res);
        assert!(text.contains("Command exited with code 1"), "got: {text}");
        assert_eq!(res.details["exit_code"], 1);
    }

    #[tokio::test]
    async fn bash_no_output() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        let tool = BashTool::new();

        // `echo` with no args yields empty stdout from the mock shell.
        let res = tool
            .execute(serde_json::json!({ "command": "echo" }), &ctx)
            .await
            .expect("empty echo should succeed");

        assert_eq!(text_of(&res), "(no output)");
        assert_eq!(res.details["exit_code"], 0);
    }

    #[tokio::test]
    async fn bash_missing_command_arg() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        let tool = BashTool::new();

        let err = tool
            .execute(serde_json::json!({ "cwd": "/tmp" }), &ctx)
            .await
            .expect_err("missing command should error");

        assert_eq!(err.code, "invalid_arguments");
        assert!(err.model_message.contains("command"));
    }

    #[tokio::test]
    async fn bash_propagates_cwd_env_timeout() {
        // Verify cwd/env/timeout reach execute_shell by inspecting the
        // RecordedShellOpts captured by MockExecutionEnv.
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env.clone());
        let tool = BashTool::new();

        tool.execute(
            serde_json::json!({
                "command": "echo hi",
                "cwd": "subdir",
                "env": { "FOO": "bar", "BAZ": "qux" },
                "timeout": 42
            }),
            &ctx,
        )
        .await
        .expect("echo should succeed");

        let opts = env
            .last_shell_opts()
            .expect("execute_shell should have been called");
        // cwd is resolved against the mock working dir (/tmp/mock).
        assert_eq!(
            opts.cwd.as_deref(),
            Some(std::path::Path::new("/tmp/mock/subdir"))
        );
        let mut env_pairs = opts.env.clone();
        env_pairs.sort();
        assert_eq!(
            env_pairs,
            vec![
                ("BAZ".to_string(), "qux".to_string()),
                ("FOO".to_string(), "bar".to_string()),
            ]
        );
        assert_eq!(opts.timeout_secs, Some(42));
    }

    #[tokio::test]
    async fn bash_timeout_clamped() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env.clone());
        let tool = BashTool::new();

        // 0 is below MIN_TIMEOUT_SECS (1) → clamped up to 1.
        tool.execute(
            serde_json::json!({ "command": "echo a", "timeout": 0 }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(env.last_shell_opts().unwrap().timeout_secs, Some(1));

        // 100_000 is above MAX_TIMEOUT_SECS (3600) → clamped down to 3600.
        tool.execute(
            serde_json::json!({ "command": "echo b", "timeout": 100_000 }),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(env.last_shell_opts().unwrap().timeout_secs, Some(3600));
    }

    #[tokio::test]
    async fn bash_shell_failed_yields_tool_result() {
        // After the mock contract fix, `false` returns Err(ShellFailed).
        // BashTool must convert that into a ToolResult (not ToolFailure), and
        // still record that execute_shell was invoked.
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env.clone());
        let tool = BashTool::new();

        let res = tool
            .execute(serde_json::json!({ "command": "false" }), &ctx)
            .await
            .expect("ShellFailed must become a ToolResult, not a ToolFailure");
        let text = text_of(&res);
        assert!(text.contains("Command exited with code 1"), "got: {text}");
        assert_eq!(res.details["exit_code"], 1);
        assert!(env.last_shell_opts().is_some());
    }

    #[tokio::test]
    async fn bash_timeout_routes_to_tool_error_with_captured_output() {
        // OsEnv signals timeout as ShellFailed { stderr: "timeout", .. }.
        // BashTool must surface it as a ToolFailure (not a ToolResult) and
        // preserve any partial stdout/stderr in the error message.
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        let tool = BashTool::new();

        let err = tool
            .execute(serde_json::json!({ "command": "__timeout__" }), &ctx)
            .await
            .expect_err("timeout must produce a ToolFailure");
        assert_eq!(err.code, "execution_failed");
        assert_eq!(err.model_message, "Tool execution failed.");
    }

    #[tokio::test]
    async fn bash_nonzero_exit_truncated_field_present() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        let tool = BashTool::new();

        let res = tool
            .execute(serde_json::json!({ "command": "false" }), &ctx)
            .await
            .expect("false should return a result");

        // `false` produces empty output → not truncated.
        assert_eq!(res.details["truncated"], false);
        assert_eq!(res.details["exit_code"], 1);
        let text = text_of(&res);
        assert!(text.contains("Command exited with code 1"));
    }

    #[tokio::test]
    async fn bash_truncated_output_readable_via_read_tool() {
        use crate::ReadTool;
        use crate::truncation::{TruncationConfig, truncate_and_persist};

        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env.clone());

        // Generate 300 lines of output.
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
        let file_path = &outcome.output[path_start..path_end];

        // Read the file using ReadTool.
        let read_tool = ReadTool::new(None);
        let read_res = read_tool
            .execute(serde_json::json!({ "path": file_path }), &ctx)
            .await
            .expect("read should succeed");

        let read_text = match read_res.model_content.as_slice() {
            [DataBlock::Text { text, .. }] => text.clone(),
            _ => panic!("expected text block"),
        };
        assert!(read_text.contains("line 1"));
        assert!(read_text.contains("line 300"));
        assert!(read_text.contains("line 150"));
    }

    #[tokio::test]
    async fn bash_long_output_truncated_end_to_end() {
        // `__seq:300` makes MockExecutionEnv emit 300 numbered lines,
        // exercising BashTool's truncation path end-to-end.
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        let tool = BashTool::new();

        let res = tool
            .execute(serde_json::json!({ "command": "__seq:300" }), &ctx)
            .await
            .expect("seq should succeed");

        assert_eq!(
            res.details["truncated"], true,
            "300-line output must be truncated"
        );
        let text = text_of(&res);
        assert!(
            text.contains("省略 200 行"),
            "missing marker with omitted count: {text}"
        );
        assert!(text.contains("line 1"), "missing head line 1: {text}");
        assert!(text.contains("line 300"), "missing tail line 300: {text}");
    }

    #[tokio::test]
    async fn bash_nonzero_exit_with_long_output_truncated() {
        // `__seq_fail:300` generates 300 lines with exit_code=1.
        // Verifies: truncation applies, exit-code line appears *after*
        // the truncation marker (marker index < exit-code line index).
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        let tool = BashTool::new();

        let res = tool
            .execute(serde_json::json!({ "command": "__seq_fail:300" }), &ctx)
            .await
            .expect("seq_fail should return a result, not an error");

        assert_eq!(res.details["truncated"], true);
        assert_eq!(res.details["exit_code"], 1);
        let text = text_of(&res);
        let marker_idx = text.find("省略 200 行").expect("missing marker");
        let exit_idx = text
            .find("Command exited with code 1")
            .expect("missing exit-code line");
        assert!(
            marker_idx < exit_idx,
            "marker must appear before exit-code line"
        );
        assert!(text.contains("line 1"), "missing head: {text}");
        assert!(text.contains("line 300"), "missing tail: {text}");
    }
}
