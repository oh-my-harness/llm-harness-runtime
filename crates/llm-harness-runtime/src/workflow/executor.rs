//! Non-LLM workflow step execution.

use std::collections::HashSet;
use std::path::{Component, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use llm_harness_types::{ExecutionEnv, ShellOptions};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::cost::CostAggregate;
use crate::error::Result;
use crate::workflow::model::{Step, StepRecord, StepResult};

pub const JSON_TRANSFORM_EXECUTOR_NAME: &str = "json_transform";
pub const SHELL_EXECUTOR_NAME: &str = "shell";

/// Context passed to a deterministic non-LLM step executor.
pub struct ExecutorCtx<'a> {
    /// Current executor step definition.
    pub current_step: &'a Step,
    /// Previous completed step result, if any.
    pub prev_result: Option<&'a StepResult>,
    /// Completed step history before this executor starts.
    pub step_history: &'a [StepRecord],
    /// Execution environment for executors that need filesystem or shell access.
    pub env: Arc<dyn ExecutionEnv>,
    /// Step-level abort token.
    pub abort: CancellationToken,
}

/// Deterministic executor for a non-LLM workflow step.
pub trait StepExecutor: Send + Sync {
    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>>;
}

const DEFAULT_SHELL_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SHELL_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Built-in deterministic executors shipped with the runtime.
pub fn builtin_executors() -> impl Iterator<Item = (&'static str, Arc<dyn StepExecutor>)> {
    [(
        JSON_TRANSFORM_EXECUTOR_NAME,
        Arc::new(JsonTransformExecutor) as Arc<dyn StepExecutor>,
    )]
    .into_iter()
}

/// JSON transform executor.
///
/// Config schema:
/// - `source`: optional, one of `auto`, `config.input`, `prev_result.structured`,
///   `prev_result.output`. Default is `auto`.
/// - `input`: JSON value used when `source = "config.input"` or by `auto`.
/// - `pointer`: optional RFC 6901 JSON Pointer applied to the selected input.
/// - `fields`: optional object mapping output field names to either a pointer string,
///   `{ "pointer": "...", "default": ... }`, or a literal value.
/// - `include_nulls`: optional bool, defaults to true. When false, null fields are omitted.
#[derive(Debug, Default)]
pub struct JsonTransformExecutor;

impl StepExecutor for JsonTransformExecutor {
    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            let config = ctx
                .current_step
                .executor_config()
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            let config_obj = config
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("json_transform config must be an object"))?;
            let input = select_input(config_obj, ctx)?;
            let transformed = if let Some(fields) = config_obj.get("fields") {
                build_fields(fields, &input, config_obj)?
            } else if let Some(pointer) = config_obj.get("pointer") {
                let pointer = pointer
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("json_transform.pointer must be a string"))?;
                read_pointer(&input, pointer)?.clone()
            } else {
                input
            };
            let output = serde_json::to_string(&transformed)?;
            Ok(StepResult {
                output,
                structured: Some(transformed),
                tool_calls_count: 0,
                session_id: String::new(),
                cost: CostAggregate::default(),
                started_at: None,
                ended_at: None,
            })
        })
    }
}

/// Policy-gated shell executor.
///
/// This executor is intentionally not registered by default. Callers must
/// explicitly inject it with `WorkflowEngine::with_executor`, usually with a
/// narrow command allowlist and a sandbox-backed `ExecutionEnv`.
#[derive(Debug, Clone)]
pub struct ShellExecutor {
    allowed_commands: HashSet<String>,
    default_timeout: Duration,
    max_output_bytes: usize,
}

impl ShellExecutor {
    pub fn new(commands: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowed_commands: commands.into_iter().map(Into::into).collect(),
            default_timeout: DEFAULT_SHELL_TIMEOUT,
            max_output_bytes: DEFAULT_SHELL_MAX_OUTPUT_BYTES,
        }
    }

    #[must_use]
    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }
}

impl StepExecutor for ShellExecutor {
    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            let config = ctx
                .current_step
                .executor_config()
                .ok_or_else(|| anyhow::anyhow!("shell config is required"))?;
            let config = config
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("shell config must be an object"))?;
            let command = required_string(config, "command")?;
            validate_shell_command_name(command)?;
            if !self.allowed_commands.contains(command) {
                anyhow::bail!("shell command '{}' is not allowlisted", command);
            }
            let args = parse_string_array(config.get("args"), "args")?;
            for arg in &args {
                validate_shell_arg(arg)?;
            }
            let env_entries = parse_env_entries(config.get("env"))?;
            let cwd = parse_shell_cwd(config.get("cwd"), ctx.env.working_dir())?;
            let timeout = config
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .map(Duration::from_millis)
                .unwrap_or(self.default_timeout);
            let cmdline = build_shell_command(command, &args);
            let env_refs = env_entries
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();
            let output = ctx
                .env
                .execute_shell(
                    &cmdline,
                    ShellOptions {
                        cwd: cwd.as_deref(),
                        env: env_refs,
                        timeout: Some(timeout),
                        abort: ctx.abort.clone(),
                        on_stdout: None,
                        on_stderr: None,
                    },
                )
                .await?;
            let (stdout, stdout_truncated) = truncate_utf8(&output.stdout, self.max_output_bytes);
            let (stderr, stderr_truncated) = truncate_utf8(&output.stderr, self.max_output_bytes);
            let structured = serde_json::json!({
                "command": command,
                "args": args,
                "exit_code": output.exit_code,
                "stdout": stdout,
                "stderr": stderr,
                "stdout_truncated": stdout_truncated,
                "stderr_truncated": stderr_truncated,
            });
            Ok(StepResult {
                output: serde_json::to_string(&structured)?,
                structured: Some(structured),
                tool_calls_count: 0,
                session_id: String::new(),
                cost: CostAggregate::default(),
                started_at: None,
                ended_at: None,
            })
        })
    }
}

fn required_string<'a>(config: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    config
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("shell config '{}' must be a non-empty string", key))
}

fn parse_string_array(value: Option<&Value>, key: &str) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let values = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("shell config '{}' must be an array", key))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToString::to_string)
                .ok_or_else(|| anyhow::anyhow!("shell config '{}[]' must be a string", key))
        })
        .collect()
}

fn parse_env_entries(value: Option<&Value>) -> Result<Vec<(String, String)>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let entries = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("shell config 'env' must be an object"))?;
    entries
        .iter()
        .map(|(key, value)| {
            if !is_safe_env_key(key) {
                anyhow::bail!("shell env key '{}' is not allowed", key);
            }
            let value = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("shell env value for '{}' must be a string", key))?;
            validate_shell_arg(value)?;
            Ok((key.clone(), value.to_string()))
        })
        .collect()
}

fn parse_shell_cwd(value: Option<&Value>, base: &std::path::Path) -> Result<Option<PathBuf>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let cwd = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("shell config 'cwd' must be a string"))?;
    if cwd.is_empty() {
        return Ok(None);
    }
    let relative = PathBuf::from(cwd);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("shell cwd must be a relative path without parent traversal");
    }
    Ok(Some(base.join(relative)))
}

fn validate_shell_command_name(command: &str) -> Result<()> {
    if command.chars().any(|ch| {
        ch.is_whitespace()
            || matches!(
                ch,
                '\0' | '"' | '\'' | '`' | '$' | '&' | '|' | ';' | '<' | '>' | '\r' | '\n'
            )
    }) {
        anyhow::bail!("shell command contains unsafe characters");
    }
    Ok(())
}

fn validate_shell_arg(arg: &str) -> Result<()> {
    if arg.chars().any(|ch| matches!(ch, '\0' | '\r' | '\n')) {
        anyhow::bail!("shell argument contains unsafe control characters");
    }
    Ok(())
}

fn is_safe_env_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && key
            .chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
}

fn build_shell_command(command: &str, args: &[String]) -> String {
    std::iter::once(command.to_string())
        .chain(args.iter().map(|arg| quote_shell_arg(arg)))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(windows)]
fn quote_shell_arg(arg: &str) -> String {
    let escaped = arg.replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(not(windows))]
fn quote_shell_arg(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_string(), true)
}

fn select_input(config: &Map<String, Value>, ctx: &ExecutorCtx<'_>) -> Result<Value> {
    let source = config
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    match source {
        "auto" => {
            if let Some(input) = config.get("input") {
                Ok(input.clone())
            } else if let Some(prev) = ctx.prev_result {
                Ok(prev
                    .structured
                    .clone()
                    .unwrap_or_else(|| Value::String(prev.output.clone())))
            } else {
                anyhow::bail!("json_transform requires config.input or a previous result")
            }
        }
        "config.input" => config
            .get("input")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("json_transform source config.input requires input")),
        "prev_result.structured" => ctx
            .prev_result
            .and_then(|prev| prev.structured.clone())
            .ok_or_else(|| anyhow::anyhow!("json_transform requires prev_result.structured")),
        "prev_result.output" => ctx
            .prev_result
            .map(|prev| Value::String(prev.output.clone()))
            .ok_or_else(|| anyhow::anyhow!("json_transform requires prev_result.output")),
        other => anyhow::bail!("json_transform source '{}' is not supported", other),
    }
}

fn build_fields(fields: &Value, input: &Value, config: &Map<String, Value>) -> Result<Value> {
    let fields = fields
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("json_transform.fields must be an object"))?;
    let include_nulls = config
        .get("include_nulls")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut output = Map::new();
    for (name, spec) in fields {
        let value = resolve_field_spec(spec, input)?;
        if include_nulls || !value.is_null() {
            output.insert(name.clone(), value);
        }
    }
    Ok(Value::Object(output))
}

fn resolve_field_spec(spec: &Value, input: &Value) -> Result<Value> {
    if let Some(pointer) = spec.as_str() {
        return Ok(read_pointer(input, pointer)?.clone());
    }
    if let Some(spec_obj) = spec.as_object() {
        if let Some(pointer) = spec_obj.get("pointer") {
            let pointer = pointer
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("json_transform field pointer must be a string"))?;
            return match read_pointer(input, pointer) {
                Ok(value) => Ok(value.clone()),
                Err(err) => {
                    if let Some(default) = spec_obj.get("default") {
                        Ok(default.clone())
                    } else {
                        Err(err)
                    }
                }
            };
        }
        if let Some(value) = spec_obj.get("value") {
            return Ok(value.clone());
        }
    }
    Ok(spec.clone())
}

fn read_pointer<'a>(input: &'a Value, pointer: &str) -> Result<&'a Value> {
    input
        .pointer(pointer)
        .ok_or_else(|| anyhow::anyhow!("json_transform pointer '{}' not found", pointer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{EnvError, FileInfo, ShellOutput};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    type EnvResult<T> = std::result::Result<T, EnvError>;

    fn executor_step(config: Value) -> Step {
        Step::executor(
            "transform",
            "Transform",
            JSON_TRANSFORM_EXECUTOR_NAME,
            Some(config),
        )
    }

    fn prev_result(structured: Option<Value>, output: impl Into<String>) -> StepResult {
        StepResult {
            output: output.into(),
            structured,
            tool_calls_count: 0,
            session_id: "prev".into(),
            cost: CostAggregate::default(),
            started_at: None,
            ended_at: None,
        }
    }

    fn noop_env() -> Arc<dyn ExecutionEnv> {
        Arc::new(llm_harness_loop::test_utils::NoOpEnv)
    }

    fn ctx_for<'a>(step: &'a Step, prev_result: Option<&'a StepResult>) -> ExecutorCtx<'a> {
        ExecutorCtx {
            current_step: step,
            prev_result,
            step_history: &[],
            env: noop_env(),
            abort: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn json_transform_selects_pointer_from_config_input() {
        let step = executor_step(serde_json::json!({
            "source": "config.input",
            "input": {"device": {"voltage": 3.3}},
            "pointer": "/device/voltage"
        }));
        let ctx = ctx_for(&step, None);

        let result = JsonTransformExecutor.execute(&ctx).await.unwrap();

        assert_eq!(result.structured, Some(serde_json::json!(3.3)));
        assert_eq!(result.output, "3.3");
    }

    #[tokio::test]
    async fn json_transform_builds_fields_from_previous_structured_result() {
        let prev = prev_result(
            Some(serde_json::json!({
                "user": {"id": 7, "name": "Ada"},
                "status": "ok"
            })),
            "ignored",
        );
        let step = executor_step(serde_json::json!({
            "fields": {
                "id": "/user/id",
                "name": {"pointer": "/user/name"},
                "missing": {"pointer": "/not/found", "default": "fallback"},
                "constant": {"value": true}
            }
        }));
        let ctx = ctx_for(&step, Some(&prev));

        let result = JsonTransformExecutor.execute(&ctx).await.unwrap();

        assert_eq!(
            result.structured,
            Some(serde_json::json!({
                "id": 7,
                "name": "Ada",
                "missing": "fallback",
                "constant": true
            }))
        );
    }

    #[tokio::test]
    async fn json_transform_rejects_missing_pointer_without_default() {
        let step = executor_step(serde_json::json!({
            "source": "config.input",
            "input": {"value": 1},
            "fields": {"bad": "/missing"}
        }));
        let ctx = ctx_for(&step, None);

        let err = JsonTransformExecutor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("pointer '/missing' not found"));
    }

    #[derive(Default)]
    struct FakeEnv {
        last_cmd: Mutex<Option<String>>,
        last_timeout: Mutex<Option<Duration>>,
        last_env: Mutex<Vec<(String, String)>>,
    }

    impl ExecutionEnv for FakeEnv {
        fn working_dir(&self) -> &Path {
            Path::new("/workspace")
        }

        fn read_text_file<'a>(
            &'a self,
            path: &'a Path,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<String>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::NotFound(path)) })
        }

        fn read_text_lines<'a>(
            &'a self,
            path: &'a Path,
            _: Option<usize>,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<Vec<String>>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::NotFound(path)) })
        }

        fn read_binary_file<'a>(
            &'a self,
            path: &'a Path,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<Vec<u8>>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::NotFound(path)) })
        }

        fn write_file<'a>(
            &'a self,
            path: &'a Path,
            _: &'a [u8],
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<()>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::PermissionDenied(path)) })
        }

        fn append_file<'a>(
            &'a self,
            path: &'a Path,
            _: &'a [u8],
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<()>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::PermissionDenied(path)) })
        }

        fn file_info<'a>(
            &'a self,
            path: &'a Path,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<FileInfo>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::NotFound(path)) })
        }

        fn list_dir<'a>(
            &'a self,
            path: &'a Path,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<Vec<FileInfo>>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::NotFound(path)) })
        }

        fn exists<'a>(
            &'a self,
            _: &'a Path,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<bool>> {
            Box::pin(async { Ok(false) })
        }

        fn create_dir<'a>(
            &'a self,
            path: &'a Path,
            _: bool,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<()>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::PermissionDenied(path)) })
        }

        fn remove<'a>(
            &'a self,
            path: &'a Path,
            _: bool,
            _: bool,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<()>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::PermissionDenied(path)) })
        }

        fn create_temp_dir<'a>(&'a self, _: &'a str) -> BoxFuture<'a, EnvResult<PathBuf>> {
            Box::pin(async { Err(EnvError::Other("not implemented".into())) })
        }

        fn execute_shell<'a>(
            &'a self,
            cmd: &'a str,
            opts: ShellOptions<'a>,
        ) -> BoxFuture<'a, EnvResult<ShellOutput>> {
            *self.last_cmd.lock().unwrap() = Some(cmd.to_string());
            *self.last_timeout.lock().unwrap() = opts.timeout;
            *self.last_env.lock().unwrap() = opts
                .env
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect();
            Box::pin(async {
                Ok(ShellOutput {
                    stdout: "ok\n".into(),
                    stderr: String::new(),
                    exit_code: 0,
                })
            })
        }

        fn cleanup<'a>(&'a self) -> BoxFuture<'a, EnvResult<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn shell_executor_rejects_command_not_allowlisted() {
        let step = Step::executor(
            "shell",
            "Shell",
            SHELL_EXECUTOR_NAME,
            Some(serde_json::json!({"command": "rm", "args": ["-rf", "target"]})),
        );
        let ctx = ctx_for(&step, None);
        let executor = ShellExecutor::new(["echo"]);

        let err = executor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("not allowlisted"));
    }

    #[tokio::test]
    async fn shell_executor_runs_allowed_command_through_env() {
        let step = Step::executor(
            "shell",
            "Shell",
            SHELL_EXECUTOR_NAME,
            Some(serde_json::json!({
                "command": "python",
                "args": ["-c", "print('ok')"],
                "env": {"RUN_MODE": "test"},
                "timeout_ms": 250
            })),
        );
        let env = Arc::new(FakeEnv::default());
        let ctx = ExecutorCtx {
            current_step: &step,
            prev_result: None,
            step_history: &[],
            env: env.clone(),
            abort: CancellationToken::new(),
        };
        let executor = ShellExecutor::new(["python"]);

        let result = executor.execute(&ctx).await.unwrap();

        assert_eq!(result.structured.as_ref().unwrap()["stdout"], "ok\n");
        assert_eq!(
            *env.last_timeout.lock().unwrap(),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            *env.last_env.lock().unwrap(),
            vec![("RUN_MODE".into(), "test".into())]
        );
        assert!(
            env.last_cmd
                .lock()
                .unwrap()
                .as_deref()
                .unwrap_or("")
                .contains("python")
        );
    }

    #[tokio::test]
    async fn shell_executor_rejects_parent_traversal_cwd() {
        let step = Step::executor(
            "shell",
            "Shell",
            SHELL_EXECUTOR_NAME,
            Some(serde_json::json!({
                "command": "echo",
                "args": ["ok"],
                "cwd": "../outside"
            })),
        );
        let ctx = ctx_for(&step, None);
        let executor = ShellExecutor::new(["echo"]);

        let err = executor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("parent traversal"));
    }
}
