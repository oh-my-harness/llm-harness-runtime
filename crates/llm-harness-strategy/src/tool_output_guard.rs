//! Fallback size-guard for tool results that return large `Full` projections.
//!
//! Implements `AfterToolCallHook`. When a tool returns `Ok(ToolResult)` with
//! `ToolSessionProjection::Full` and the combined text content exceeds a
//! configurable threshold, the hook:
//!
//! 1. Persists the full text to a temp file via `ExecutionEnv`.
//! 2. Replaces `model_content` with a truncated head + marker + tail.
//! 3. Switches `session_projection` to `Projected` so the truncated text is
//!    frozen in the session (satisfying the "replacement string frozen on
//!    first appearance" cache constraint from ch2).
//!
//! ## What it does *not* touch
//!
//! - Results already using `Projected` or `Ephemeral` — the tool already
//!   decided its projection strategy.
//! - Results where `details["truncated"] == true` — the tool already
//!   truncated (e.g. `BashTool`).
//! - Non-text blocks (`Image`, `Document`) — only `DataBlock::Text` is
//!   considered for size and truncation.
//! - Tools listed in `skip_tools`.
//!
//! ## Degradation
//!
//! If `ExecutionEnv` is unavailable or temp-file creation fails, the hook
//! returns `Passthrough` — the original result is used unchanged. Never
//! discards data because persistence failed.
//!
//! `ToolOutputGuardPlugin` wraps the hook as a `Plugin` for one-line install.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::{HarnessHooks, Plugin};
use llm_harness_types::{
    AfterToolCallCtx, AfterToolCallDecision, AfterToolCallHook, DataBlock, EnvError, ExecutionEnv,
    ToolResult, ToolSessionProjection,
};
use tokio_util::sync::CancellationToken;

// ── Config ───────────────────────────────────────────────────────────────────

/// Defaults follow the tutorial (ch4 line 247-252) and match `BashTool`'s
/// `TruncationConfig`: trigger at >200 lines or >10000 bytes; keep first 50
/// and last 50 lines.
const DEFAULT_MAX_LINES: usize = 200;
const DEFAULT_MAX_BYTES: usize = 10_000;
const DEFAULT_HEAD_LINES: usize = 50;
const DEFAULT_TAIL_LINES: usize = 50;

/// Configuration for [`ToolOutputGuardHook`].
#[derive(Debug, Clone)]
pub struct ToolOutputGuardConfig {
    /// Trigger truncation when line count strictly exceeds this.
    pub max_lines: usize,
    /// Trigger truncation when byte count strictly exceeds this.
    pub max_bytes: usize,
    /// Lines to retain from the head when truncating.
    pub head_lines: usize,
    /// Lines to retain from the tail when truncating.
    pub tail_lines: usize,
    /// Tool names to skip entirely (e.g. tools that manage their own output).
    pub skip_tools: HashSet<String>,
}

impl Default for ToolOutputGuardConfig {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
            head_lines: DEFAULT_HEAD_LINES,
            tail_lines: DEFAULT_TAIL_LINES,
            skip_tools: HashSet::new(),
        }
    }
}

// ── Hook ─────────────────────────────────────────────────────────────────────

/// `AfterToolCallHook` that truncates large `Full`-projection tool results.
///
/// Inject `ExecutionEnv` at construction time (recommended). If `None`, the
/// hook falls back to `RunExtensions` lookup (same pattern as `SafetyHook`).
///
/// # Example
///
/// ```ignore
/// use llm_harness_strategy::ToolOutputGuardHook;
/// use std::sync::Arc;
///
/// let env: Arc<dyn ExecutionEnv> = /* ... */;
/// let hook = ToolOutputGuardHook::new(env);
/// ```
pub struct ToolOutputGuardHook {
    env: Option<Arc<dyn ExecutionEnv>>,
    config: ToolOutputGuardConfig,
}

impl ToolOutputGuardHook {
    /// Create with an injected `ExecutionEnv` and default config.
    pub fn new(env: Arc<dyn ExecutionEnv>) -> Self {
        Self {
            env: Some(env),
            config: ToolOutputGuardConfig::default(),
        }
    }

    /// Create with an injected `ExecutionEnv` and custom config.
    pub fn with_config(env: Arc<dyn ExecutionEnv>, config: ToolOutputGuardConfig) -> Self {
        Self {
            env: Some(env),
            config,
        }
    }

    /// Create without an injected env; will try `RunExtensions` at runtime.
    /// If env is not found, the hook degrades to `Passthrough`.
    pub fn from_run_extensions() -> Self {
        Self {
            env: None,
            config: ToolOutputGuardConfig::default(),
        }
    }

    /// Resolve the execution environment: injected first, then RunExtensions.
    fn resolve_env(&self, run: &llm_harness_types::RunContext) -> Option<Arc<dyn ExecutionEnv>> {
        if let Some(env) = &self.env {
            return Some(env.clone());
        }
        // extension_arc::<Arc<dyn ExecutionEnv>>() returns Arc<Arc<dyn ExecutionEnv>>;
        // dereference to get the inner Arc.
        run.extension_arc::<Arc<dyn ExecutionEnv>>()
            .map(|arc| (*arc).clone())
    }
}

impl AfterToolCallHook for ToolOutputGuardHook {
    fn on_complete<'a>(
        &'a self,
        ctx: AfterToolCallCtx<'a>,
    ) -> BoxFuture<'a, AfterToolCallDecision> {
        Box::pin(async move {
            // ── Guard conditions ───────────────────────────────────────
            let result = match ctx.result {
                Ok(r) => r,
                Err(_) => return AfterToolCallDecision::Passthrough,
            };

            // Skip tools in the skip list.
            if self.config.skip_tools.contains(ctx.tool_name) {
                return AfterToolCallDecision::Passthrough;
            }

            // Only guard Full projections — Projected/Ephemeral are already
            // managed by the tool.
            if !matches!(
                result.session_projection,
                ToolSessionProjection::Full { .. }
            ) {
                return AfterToolCallDecision::Passthrough;
            }

            // Skip if the tool already truncated (defensive — avoids
            // double-truncation when a tool truncates but keeps Full
            // projection, as BashTool does).
            if result.details.get("truncated").and_then(|v| v.as_bool()) == Some(true) {
                return AfterToolCallDecision::Passthrough;
            }

            // Only handle results that are entirely Text blocks.
            // Mixed results with Image/Document are passed through —
            // truncating text next to binary content is semantically wrong.
            if result.model_content.is_empty()
                || result
                    .model_content
                    .iter()
                    .any(|b| !matches!(b, DataBlock::Text { .. }))
            {
                return AfterToolCallDecision::Passthrough;
            }

            // ── Size check ─────────────────────────────────────────────
            let combined: String = result
                .model_content
                .iter()
                .filter_map(|b| match b {
                    DataBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");

            // Collect lines once — used for both size check and truncation.
            let lines: Vec<&str> = combined.lines().collect();

            let exceeds_bytes = combined.len() > self.config.max_bytes;
            if !exceeds_bytes && lines.len() <= self.config.max_lines {
                return AfterToolCallDecision::Passthrough;
            }

            // Don't truncate if too few lines for head+tail to be meaningful.
            if lines.len() <= self.config.head_lines + self.config.tail_lines {
                return AfterToolCallDecision::Passthrough;
            }

            // ── Resolve env ────────────────────────────────────────────
            let env = match self.resolve_env(ctx.run) {
                Some(env) => env,
                None => {
                    tracing::warn!(
                        tool = ctx.tool_name,
                        "ToolOutputGuardHook: ExecutionEnv not available; \
                         passing through large result untruncated"
                    );
                    return AfterToolCallDecision::Passthrough;
                }
            };

            // ── Persist + truncate ─────────────────────────────────────
            let outcome = match truncate_and_persist(
                &combined,
                &env,
                &self.config,
                ctx.run.id().to_string().as_str(),
            )
            .await
            {
                Ok(o) if o.truncated => o,
                Ok(_) => return AfterToolCallDecision::Passthrough,
                Err(e) => {
                    tracing::warn!(
                        tool = ctx.tool_name,
                        error = %e,
                        "ToolOutputGuardHook: persistence failed; \
                         passing through large result untruncated"
                    );
                    return AfterToolCallDecision::Passthrough;
                }
            };

            // ── Build replacement result ───────────────────────────────
            // Use Projected so the truncated text is frozen in session.
            let truncated_block = DataBlock::text(outcome.output);
            // Mark details["truncated"] = true so downstream hooks know the
            // content was already truncated (defensive — avoids double-
            // truncation if a future hook has similar logic).
            let mut details = result.details.clone();
            details["truncated"] = serde_json::json!(true);
            let new_result = ToolResult {
                model_content: vec![truncated_block.clone()],
                details,
                extensions: result.extensions.clone(),
                terminate: result.terminate,
                session_projection: ToolSessionProjection::projected(vec![truncated_block]),
            };

            AfterToolCallDecision::Replace(Ok(new_result))
        })
    }
}

// ── Plugin ───────────────────────────────────────────────────────────────────

/// Plugin that installs [`ToolOutputGuardHook`].
///
/// # Example
///
/// ```ignore
/// use llm_harness_strategy::ToolOutputGuardPlugin;
/// use std::sync::Arc;
///
/// let env: Arc<dyn ExecutionEnv> = /* ... */;
/// let harness = HarnessBuilder::new("claude-3-5-sonnet")
///     .install(&ToolOutputGuardPlugin::new(env))
///     .build(env)
///     .await?;
/// ```
pub struct ToolOutputGuardPlugin {
    hook: Arc<ToolOutputGuardHook>,
}

impl ToolOutputGuardPlugin {
    /// Create with an injected `ExecutionEnv` and default config.
    pub fn new(env: Arc<dyn ExecutionEnv>) -> Self {
        Self {
            hook: Arc::new(ToolOutputGuardHook::new(env)),
        }
    }

    /// Create with an injected `ExecutionEnv` and custom config.
    pub fn with_config(env: Arc<dyn ExecutionEnv>, config: ToolOutputGuardConfig) -> Self {
        Self {
            hook: Arc::new(ToolOutputGuardHook::with_config(env, config)),
        }
    }
}

impl Plugin for ToolOutputGuardPlugin {
    fn name(&self) -> &str {
        "tool-output-guard"
    }

    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        hooks.after_tool_call.push(self.hook.clone());
    }
}

// ── Truncation helper (self-contained, mirrors runtime-tools' logic) ─────────
//
// strategy crate does not depend on runtime-tools, so the ~80 lines of
// truncation logic are replicated here. The alternative (making strategy
// depend on runtime-tools) would invert the dependency direction.

/// Outcome of [`truncate_and_persist`].
struct TruncationOutcome {
    /// Final text for the model (may contain truncation marker).
    output: String,
    /// Whether truncation was applied.
    truncated: bool,
}

/// Check whether truncation is needed, and if so, persist the full output to
/// a temp file and return a truncated version (head + marker + tail).
///
/// **Degradation:** if `create_temp_dir` or `write_file` fails, returns
/// `Ok(TruncationOutcome { truncated: false })` — never discards the result
/// just because persistence failed.
async fn truncate_and_persist(
    raw_output: &str,
    env: &Arc<dyn ExecutionEnv>,
    config: &ToolOutputGuardConfig,
    run_id: &str,
) -> Result<TruncationOutcome, EnvError> {
    let lines: Vec<&str> = raw_output.lines().collect();

    let temp_dir = match env.create_temp_dir("harness_output").await {
        Ok(dir) => dir,
        Err(_) => {
            return Ok(TruncationOutcome {
                output: raw_output.to_string(),
                truncated: false,
            });
        }
    };

    // Use run_id in filename to avoid collisions across concurrent runs.
    let file_path: PathBuf = temp_dir.join(format!("output_{}.txt", run_id));
    let abort = CancellationToken::new();
    if env
        .write_file(&file_path, raw_output.as_bytes(), abort)
        .await
        .is_err()
    {
        return Ok(TruncationOutcome {
            output: raw_output.to_string(),
            truncated: false,
        });
    }

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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{
        AssistantMessage, DataBlock, RunContext, RunRequest, ToolFailure, ToolResult,
    };
    use std::path::Path;

    fn make_run_context() -> RunContext {
        RunContext::new(RunRequest::from_text("test"))
    }

    fn make_assistant() -> AssistantMessage {
        llm_harness_loop::test_utils::test_assistant_message(vec![])
    }

    fn make_after_ctx<'a>(
        run: &'a RunContext,
        assistant: &'a AssistantMessage,
        tool_use_id: &'a str,
        tool_name: &'a str,
        result: &'a Result<ToolResult, ToolFailure>,
    ) -> AfterToolCallCtx<'a> {
        AfterToolCallCtx {
            run,
            assistant_message: assistant,
            tool_use_id,
            tool_name,
            args: &serde_json::Value::Null,
            result,
            duration: std::time::Duration::from_millis(42),
            turn_index: 0,
        }
    }

    /// Test env backed by real filesystem via `tempfile`.
    struct TestEnv {
        root: PathBuf,
        /// When true, `create_temp_dir` returns an error (degradation test).
        fail_temp_dir: std::sync::atomic::AtomicBool,
        /// When true, `write_file` returns an error (degradation test).
        fail_write: std::sync::atomic::AtomicBool,
    }

    impl TestEnv {
        fn new(root: &Path) -> Self {
            Self {
                root: root.to_path_buf(),
                fail_temp_dir: std::sync::atomic::AtomicBool::new(false),
                fail_write: std::sync::atomic::AtomicBool::new(false),
            }
        }

        /// Inject a failure into `create_temp_dir`.
        fn set_fail_temp_dir(&self, fail: bool) {
            self.fail_temp_dir
                .store(fail, std::sync::atomic::Ordering::Relaxed);
        }

        /// Inject a failure into `write_file`.
        fn set_fail_write(&self, fail: bool) {
            self.fail_write
                .store(fail, std::sync::atomic::Ordering::Relaxed);
        }
    }

    impl ExecutionEnv for TestEnv {
        fn working_dir(&self) -> &Path {
            &self.root
        }

        fn read_text_file<'a>(
            &'a self,
            path: &'a Path,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<String, EnvError>> {
            Box::pin(async move { std::fs::read_to_string(path).map_err(EnvError::Io) })
        }

        fn read_text_lines<'a>(
            &'a self,
            path: &'a Path,
            max_lines: Option<usize>,
            abort: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<String>, EnvError>> {
            let text = self.read_text_file(path, abort);
            Box::pin(async move {
                let text = text.await?;
                let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
                Ok(match max_lines {
                    Some(max) => lines.into_iter().take(max).collect(),
                    None => lines,
                })
            })
        }

        fn read_binary_file<'a>(
            &'a self,
            path: &'a Path,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<u8>, EnvError>> {
            Box::pin(async move { std::fs::read(path).map_err(EnvError::Io) })
        }

        fn write_file<'a>(
            &'a self,
            path: &'a Path,
            content: &'a [u8],
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<(), EnvError>> {
            Box::pin(async move {
                if self.fail_write.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(EnvError::Other("injected write failure".into()));
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(EnvError::Io)?;
                }
                std::fs::write(path, content).map_err(EnvError::Io)
            })
        }

        fn append_file<'a>(
            &'a self,
            path: &'a Path,
            content: &'a [u8],
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<(), EnvError>> {
            Box::pin(async move {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map_err(EnvError::Io)?;
                f.write_all(content).map_err(EnvError::Io)
            })
        }

        fn file_info<'a>(
            &'a self,
            path: &'a Path,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<llm_harness_types::FileInfo, EnvError>> {
            Box::pin(async move {
                let meta = std::fs::metadata(path).map_err(EnvError::Io)?;
                Ok(llm_harness_types::FileInfo {
                    path: path.to_path_buf(),
                    size: meta.len(),
                    is_dir: meta.is_dir(),
                    modified: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0)
                        .unwrap_or_default(),
                })
            })
        }

        fn list_dir<'a>(
            &'a self,
            path: &'a Path,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<llm_harness_types::FileInfo>, EnvError>> {
            Box::pin(async move {
                let entries = std::fs::read_dir(path).map_err(EnvError::Io)?;
                let mut result = Vec::new();
                for entry in entries {
                    let entry = entry.map_err(EnvError::Io)?;
                    let meta = entry.metadata().map_err(EnvError::Io)?;
                    result.push(llm_harness_types::FileInfo {
                        path: entry.path(),
                        size: meta.len(),
                        is_dir: meta.is_dir(),
                        modified: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0)
                            .unwrap_or_default(),
                    });
                }
                Ok(result)
            })
        }

        fn exists<'a>(
            &'a self,
            path: &'a Path,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<bool, EnvError>> {
            Box::pin(async move { Ok(path.exists()) })
        }

        fn create_dir<'a>(
            &'a self,
            path: &'a Path,
            recursive: bool,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<(), EnvError>> {
            Box::pin(async move {
                if recursive {
                    std::fs::create_dir_all(path)
                } else {
                    std::fs::create_dir(path)
                }
                .map_err(EnvError::Io)
            })
        }

        fn remove<'a>(
            &'a self,
            path: &'a Path,
            recursive: bool,
            _force: bool,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<(), EnvError>> {
            Box::pin(async move {
                if recursive {
                    std::fs::remove_dir_all(path)
                } else {
                    std::fs::remove_file(path)
                }
                .map_err(EnvError::Io)
            })
        }
        fn create_temp_dir<'a>(
            &'a self,
            prefix: &'a str,
        ) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
            Box::pin(async move {
                if self
                    .fail_temp_dir
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    return Err(EnvError::Other("injected temp-dir failure".into()));
                }
                tempfile::TempDir::with_prefix(prefix)
                    .map(|t| t.keep())
                    .map_err(EnvError::Io)
            })
        }

        fn execute_shell<'a>(
            &'a self,
            _cmd: &'a str,
            _opts: llm_harness_types::ShellOptions<'a>,
        ) -> BoxFuture<'a, Result<llm_harness_types::ShellOutput, EnvError>> {
            Box::pin(async move { Err(EnvError::Other("TestEnv does not support shell".into())) })
        }

        fn cleanup<'a>(&'a self) -> BoxFuture<'a, Result<(), EnvError>> {
            Box::pin(async move { Ok(()) })
        }
    }

    // ── Tests: guard conditions ───────────────────────────────────────

    #[tokio::test]
    async fn passthrough_for_small_result() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));
        let hook = ToolOutputGuardHook::new(env);

        let run = make_run_context();
        let assistant = make_assistant();
        let result = Ok(ToolResult::full(
            vec![DataBlock::text("hello")],
            serde_json::json!({}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
    }

    #[tokio::test]
    async fn passthrough_for_projected_result() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));
        let hook = ToolOutputGuardHook::new(env);

        let run = make_run_context();
        let assistant = make_assistant();
        // Already projected — tool managed its own projection.
        let result = Ok(ToolResult::projected(
            vec![DataBlock::text("x".repeat(20_000))],
            vec![DataBlock::text("summary")],
            serde_json::json!({}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
    }

    #[tokio::test]
    async fn passthrough_for_ephemeral_result() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));
        let hook = ToolOutputGuardHook::new(env);

        let run = make_run_context();
        let assistant = make_assistant();
        let result = Ok(ToolResult::ephemeral(
            vec![DataBlock::text("x".repeat(20_000))],
            "summary",
            serde_json::json!({}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
    }

    #[tokio::test]
    async fn passthrough_when_details_truncated_true() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));
        let hook = ToolOutputGuardHook::new(env);

        let run = make_run_context();
        let assistant = make_assistant();
        // BashTool-style: already truncated, Full projection.
        let result = Ok(ToolResult::full(
            vec![DataBlock::text("x".repeat(20_000))],
            serde_json::json!({"truncated": true}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "bash", &result);

        let decision = hook.on_complete(ctx).await;
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
    }

    #[tokio::test]
    async fn passthrough_for_skipped_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));
        let mut config = ToolOutputGuardConfig::default();
        config.skip_tools.insert("my_tool".into());
        let hook = ToolOutputGuardHook::with_config(env, config);

        let run = make_run_context();
        let assistant = make_assistant();
        let result = Ok(ToolResult::full(
            vec![DataBlock::text("x".repeat(20_000))],
            serde_json::json!({}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
    }

    #[tokio::test]
    async fn passthrough_for_error_result() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));
        let hook = ToolOutputGuardHook::new(env);

        let run = make_run_context();
        let assistant = make_assistant();
        let result: Result<ToolResult, ToolFailure> =
            Err(ToolFailure::new("err", "something went wrong"));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
    }

    #[tokio::test]
    async fn passthrough_for_mixed_content_with_image() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));
        let hook = ToolOutputGuardHook::new(env);

        let run = make_run_context();
        let assistant = make_assistant();
        let result = Ok(ToolResult::full(
            vec![
                DataBlock::text("x".repeat(20_000)),
                DataBlock::Image {
                    source: llm_harness_types::ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "iVBOR".into(),
                    },
                },
            ],
            serde_json::json!({}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
    }

    #[tokio::test]
    async fn passthrough_when_env_not_available() {
        // No env injected, no env in RunExtensions.
        let hook = ToolOutputGuardHook::from_run_extensions();

        let run = make_run_context();
        let assistant = make_assistant();
        let result = Ok(ToolResult::full(
            vec![DataBlock::text("x".repeat(20_000))],
            serde_json::json!({}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
    }

    // ── Tests: truncation behavior ────────────────────────────────────

    #[tokio::test]
    async fn truncates_large_full_result() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));
        let hook = ToolOutputGuardHook::new(env);

        let run = make_run_context();
        let assistant = make_assistant();
        // 300 lines, each ~10 bytes → 3000 bytes (under byte threshold)
        // but 300 lines > 200 line threshold.
        let big_text: String = (0..300)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = Ok(ToolResult::full(
            vec![DataBlock::text(big_text.clone())],
            serde_json::json!({}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;

        let AfterToolCallDecision::Replace(Ok(replaced)) = decision else {
            panic!("expected Replace(Ok), got something else");
        };

        // model_content should be truncated.
        let text = match &replaced.model_content[0] {
            DataBlock::Text { text, .. } => text.as_str(),
            _ => panic!("expected Text block"),
        };
        assert!(text.contains("省略 200 行"));
        assert!(text.contains("完整输出已保存至"));

        // session_projection should be Projected (not Full).
        assert!(matches!(
            replaced.session_projection,
            ToolSessionProjection::Projected { .. }
        ));

        // Head and tail should be present.
        assert!(text.contains("line 0")); // head
        assert!(text.contains("line 299")); // tail
        // Middle should be omitted.
        assert!(!text.contains("line 150"));

        // details should be marked truncated.
        assert_eq!(replaced.details["truncated"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn passthrough_when_too_few_lines_for_head_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));
        let hook = ToolOutputGuardHook::new(env);

        let run = make_run_context();
        let assistant = make_assistant();
        // 10 lines but >10000 bytes — exceeds byte threshold.
        let big_text = "x".repeat(15_000);
        let result = Ok(ToolResult::full(
            vec![DataBlock::text(big_text)],
            serde_json::json!({}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;

        // 10 lines but head+tail=100 → line count too small, won't truncate.
        // This is correct: head+tail would be the entire content.
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
    }

    #[tokio::test]
    async fn truncates_multi_block_text_result() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));
        let hook = ToolOutputGuardHook::new(env);

        let run = make_run_context();
        let assistant = make_assistant();
        // Two text blocks, combined exceeds line threshold.
        let block1: String = (0..150)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let block2: String = (0..150)
            .map(|i| format!("more {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = Ok(ToolResult::full(
            vec![DataBlock::text(block1), DataBlock::text(block2)],
            serde_json::json!({}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;

        let AfterToolCallDecision::Replace(Ok(replaced)) = decision else {
            panic!("expected Replace(Ok), got something else");
        };

        // Should be a single text block after truncation.
        assert_eq!(replaced.model_content.len(), 1);
        let text = match &replaced.model_content[0] {
            DataBlock::Text { text, .. } => text.as_str(),
            _ => panic!("expected Text block"),
        };
        assert!(text.contains("省略"));
        assert!(matches!(
            replaced.session_projection,
            ToolSessionProjection::Projected { .. }
        ));
    }

    #[tokio::test]
    async fn preserves_terminate_and_details() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));
        let hook = ToolOutputGuardHook::new(env);

        let run = make_run_context();
        let assistant = make_assistant();
        let big_text: String = (0..300)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = Ok(ToolResult::full(
            vec![DataBlock::text(big_text)],
            serde_json::json!({"custom": "metadata"}),
            true, // terminate=true
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;

        let AfterToolCallDecision::Replace(Ok(replaced)) = decision else {
            panic!("expected Replace(Ok)");
        };
        assert!(replaced.terminate);
        assert_eq!(replaced.details["custom"], "metadata");
        assert_eq!(replaced.details["truncated"], serde_json::json!(true));
    }
    #[tokio::test]
    async fn temp_file_persisted_and_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));
        let hook = ToolOutputGuardHook::new(env.clone());

        let run = make_run_context();
        let assistant = make_assistant();
        let big_text: String = (0..300)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = Ok(ToolResult::full(
            vec![DataBlock::text(big_text.clone())],
            serde_json::json!({}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;

        let AfterToolCallDecision::Replace(Ok(replaced)) = decision else {
            panic!("expected Replace(Ok)");
        };

        let text = match &replaced.model_content[0] {
            DataBlock::Text { text, .. } => text.as_str(),
            _ => panic!("expected Text block"),
        };

        // Extract file path from marker.
        let path_start = text.find("已保存至 `").unwrap() + "已保存至 `".len();
        let path_end = text[path_start..].find('`').unwrap() + path_start;
        let file_path = &text[path_start..path_end];

        // Read back the persisted file.
        let abort = CancellationToken::new();
        let content = env
            .read_text_file(std::path::Path::new(file_path), abort)
            .await
            .unwrap();
        assert_eq!(content, big_text);
    }

    #[tokio::test]
    async fn env_from_run_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));

        // Inject env into RunExtensions.
        let mut request = RunRequest::from_text("test");
        request
            .extensions
            .insert(Arc::clone(&env) as Arc<dyn ExecutionEnv>);
        let run = RunContext::new(request);

        let hook = ToolOutputGuardHook::from_run_extensions();
        let assistant = make_assistant();
        let big_text: String = (0..300)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = Ok(ToolResult::full(
            vec![DataBlock::text(big_text)],
            serde_json::json!({}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;
        assert!(matches!(decision, AfterToolCallDecision::Replace(Ok(_))));
    }

    #[tokio::test]
    async fn empty_result_passthrough() {
        let tmp = tempfile::tempdir().unwrap();
        let env: Arc<dyn ExecutionEnv> = Arc::new(TestEnv::new(tmp.path()));
        let hook = ToolOutputGuardHook::new(env);

        let run = make_run_context();
        let assistant = make_assistant();
        let result = Ok(ToolResult::full(vec![], serde_json::json!({}), false));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
    }

    #[tokio::test]
    async fn degrades_when_create_temp_dir_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let env = Arc::new(TestEnv::new(tmp.path()));
        env.set_fail_temp_dir(true);
        let hook = ToolOutputGuardHook::new(env);

        let run = make_run_context();
        let assistant = make_assistant();
        let big_text: String = (0..300)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = Ok(ToolResult::full(
            vec![DataBlock::text(big_text)],
            serde_json::json!({}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;
        // Persistence failed → must pass through, never discard data.
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
    }

    #[tokio::test]
    async fn degrades_when_write_file_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let env = Arc::new(TestEnv::new(tmp.path()));
        env.set_fail_write(true);
        let hook = ToolOutputGuardHook::new(env);

        let run = make_run_context();
        let assistant = make_assistant();
        let big_text: String = (0..300)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = Ok(ToolResult::full(
            vec![DataBlock::text(big_text)],
            serde_json::json!({}),
            false,
        ));
        let ctx = make_after_ctx(&run, &assistant, "tu-1", "my_tool", &result);

        let decision = hook.on_complete(ctx).await;
        // Persistence failed → must pass through, never discard data.
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
    }
}
