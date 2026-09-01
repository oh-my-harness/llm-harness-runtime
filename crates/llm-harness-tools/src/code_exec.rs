use std::time::Duration;

use futures::future::BoxFuture;
use llm_harness_types::{
    DataBlock, EnvError, ShellOptions, ShellOutput, Tool, ToolContext, ToolFailure, ToolResult,
};
use serde_json::json;

static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "language": {
                "type": "string",
                "enum": ["python", "bash", "javascript"],
                "description": "Programming language. e.g. \"python\", \"bash\", \"javascript\""
            },
            "code": {
                "type": "string",
                "description": "Code to execute. e.g. \"print('hello')\" or \"console.log('hi')\""
            }
        },
        "required": ["language", "code"]
    })
});

/// Execute user-supplied code in a temporary directory via the execution env.
pub struct CodeExecTool {
    timeout: Duration,
}

impl CodeExecTool {
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Default for CodeExecTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for CodeExecTool {
    fn name(&self) -> &str {
        "code_exec"
    }

    fn report_duration(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Execute code in a temporary directory and return stdout, stderr, and exit code.\n\n\
         Use when: running quick computations or data processing; testing code snippets; generating output that feeds into later steps; performing operations not covered by other tools.\n\
         Don't use for: running shell commands like git or ls (use bash); reading files (use read); writing files (use write); long-running processes or daemons; installing packages.\n\
         Returns: exit_code, stdout, and stderr. Non-zero exit codes are NOT tool errors — check exit_code in the output.\n\n\
         Examples:\n\
         {\"language\": \"python\", \"code\": \"print(2 ** 10)\"}\n\
         {\"language\": \"python\", \"code\": \"import json; print(json.dumps({\\\"key\\\": \\\"value\\\"}))\"}\n\
         {\"language\": \"bash\", \"code\": \"echo hello && date\"}\n\
         {\"language\": \"javascript\", \"code\": \"console.log(Array.from({length: 5}, (_, i) => i * 2))\"}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &SCHEMA
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let language = args["language"].as_str().unwrap_or("bash");
            let code = args["code"]
                .as_str()
                .ok_or_else(|| ToolFailure::invalid_arguments("missing code"))?;

            // Create a temp working directory
            let work_dir = ctx
                .env
                .create_temp_dir("code_exec")
                .await
                .map_err(|e| ToolFailure::new("execution_failed", e.to_string()))?;

            // Write source file
            let (filename, shell_cmd): (&str, String) = match language {
                "python" => ("script.py", format!("{} script.py", python_command())),
                "bash" => ("script.sh", "bash script.sh".to_string()),
                "javascript" => ("script.js", format!("{} script.js", js_command())),
                other => {
                    return Err(ToolFailure::invalid_arguments(format!(
                        "unsupported language: {other}"
                    )));
                }
            };

            ctx.env
                .write_file(&work_dir.join(filename), code.as_bytes(), ctx.abort.clone())
                .await
                .map_err(|e| ToolFailure::new("execution_failed", e.to_string()))?;

            // Execute through the env's sanctioned shell path — this respects
            // sandbox isolation (bwrap/seatbelt in production), abort signals,
            // and timeout. Non-zero exit is NOT a system error.
            let opts = ShellOptions {
                cwd: Some(&work_dir),
                env: vec![],
                timeout: Some(self.timeout),
                abort: ctx.abort.clone(),
                on_stdout: None,
                on_stderr: None,
                max_output_bytes: None,
            };

            let output = match ctx.env.execute_shell(&shell_cmd, opts).await {
                Ok(o) => o,
                Err(EnvError::Aborted) => return Err(ToolFailure::aborted()),
                Err(EnvError::ShellTimeout { .. }) => {
                    return Err(ToolFailure::new(
                        "execution_timeout",
                        "Code execution timed out.",
                    ));
                }
                Err(EnvError::ShellFailed {
                    exit_code,
                    stdout,
                    stderr,
                    stdout_truncated_bytes,
                    stderr_truncated_bytes,
                }) => ShellOutput {
                    stdout,
                    stderr,
                    exit_code,
                    stdout_truncated_bytes,
                    stderr_truncated_bytes,
                },
                Err(e) => {
                    return Err(ToolFailure::new("execution_failed", e.to_string()));
                }
            };

            let stdout = output.stdout;
            let stderr = output.stderr;
            let exit_code = output.exit_code;

            let output_text = format!(
                "exit_code: {exit_code}\n\
                 stdout:\n{stdout}\
                 {}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!("stderr:\n{stderr}")
                }
            );

            let model_content = vec![DataBlock::text(output_text)];
            Ok(ToolResult::projected(
                model_content.clone(),
                model_content,
                json!({
                    "language": language,
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                }),
                false,
            ))
        })
    }
}

fn python_command() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

/// Detect the JavaScript runtime once and cache it.
static JS_RUNTIME: std::sync::LazyLock<&'static str> = std::sync::LazyLock::new(|| {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok()
    {
        "node"
    } else {
        "bun"
    }
});

fn js_command() -> &'static str {
    *JS_RUNTIME
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_sandbox::os::OsEnv;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn make_ctx(tmp: &std::path::Path) -> ToolContext {
        let (tx, _rx) = mpsc::channel(1);
        ToolContext {
            env: Arc::new(OsEnv::new(tmp)),
            run: Arc::new(llm_harness_types::RunContext::new(
                llm_harness_types::RunRequest::default(),
            )),
            abort: CancellationToken::new(),
            tool_use_id: "test-id".into(),
            turn_index: 0,
            assistant_message: Arc::new(llm_harness_types::AssistantMessage {
                kind: llm_harness_types::AssistantMessageKind::FinalAnswer,
                message_id: "test-message".into(),
                turn_id: "test-turn".into(),
                content: vec![],
                usage: None,
                stop_reason: None,
                timestamp: chrono::Utc::now(),
                provider: None,
                api: None,
                model: None,
                error_message: None,
            }),
            update_tx: tx,
        }
    }

    #[tokio::test]
    async fn python_hello_world() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = CodeExecTool::new();
        let args = serde_json::json!({
            "language": "python",
            "code": "print('hello from test')"
        });
        let result = tool.execute(args, &make_ctx(tmp.path())).await.unwrap();
        let text = match &result.model_content[0] {
            llm_harness_types::DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("hello from test"), "got: {text}");
        assert_eq!(result.details["exit_code"], 0);
    }

    #[tokio::test]
    async fn nonzero_exit_is_not_tool_error() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = CodeExecTool::new();
        let args = serde_json::json!({
            "language": "python",
            "code": "import sys; sys.exit(1)"
        });
        let result = tool.execute(args, &make_ctx(tmp.path())).await.unwrap();
        assert_eq!(result.details["exit_code"], 1);
    }

    #[tokio::test]
    async fn code_exec_runs_javascript() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = CodeExecTool::new();
        let args = serde_json::json!({
            "language": "javascript",
            "code": "console.log('hello from js')"
        });
        let result = tool.execute(args, &make_ctx(tmp.path())).await;
        if result.is_err() {
            eprintln!("JavaScript runtime not available, skipping");
            return;
        }
        let result = result.unwrap();
        let text = match &result.model_content[0] {
            llm_harness_types::DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("hello from js"), "got: {text}");
    }

    #[test]
    fn with_timeout_sets_custom_duration() {
        let tool = CodeExecTool::with_timeout(Duration::from_secs(60));
        assert_eq!(tool.timeout, Duration::from_secs(60));
    }
}
