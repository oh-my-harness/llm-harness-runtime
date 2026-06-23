use std::sync::OnceLock;

use futures::future::BoxFuture;
use llm_harness_types::*;
use serde_json::Value;

use super::MAX_OUTPUT_BYTES;

/// Search for a regex pattern in files using ripgrep (falls back to grep).
///
/// Respects .gitignore. Returns matching lines with line numbers and
/// optional context. Output is truncated when it exceeds MAX_OUTPUT_BYTES.
pub struct GrepTool {
    schema: Value,
}

impl GrepTool {
    pub fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression pattern to search for"
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory to search in (default: working directory)"
                    },
                    "include": {
                        "type": "string",
                        "description": "Glob pattern to include (e.g. '*.rs')"
                    },
                    "context": {
                        "type": "integer",
                        "description": "Lines of context around each match"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }
}

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search for a regex pattern in files using ripgrep. \
         Respects .gitignore. Use include to filter by file type. \
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
        Box::pin(async move { run_grep(args, ctx).await })
    }
}

async fn run_grep(args: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("pattern required".into()))?;

    let search_path = args["path"].as_str().unwrap_or(".");
    let include = args["include"].as_str();
    let context_lines = args["context"].as_u64().unwrap_or(0);

    let use_rg = has_ripgrep();
    let cmd = if use_rg {
        build_rg_command(pattern, search_path, include, context_lines)
    } else {
        build_grep_command(pattern, search_path, include, context_lines)
    };

    let opts = ShellOptions {
        cwd: Some(ctx.env.working_dir()),
        env: vec![],
        timeout: Some(std::time::Duration::from_secs(30)),
        abort: ctx.abort.clone(),
        on_stdout: None,
        on_stderr: None,
    };

    let output = ctx.env.execute_shell(&cmd, opts).await;
    // rg/grep exits 1 when no matches — treat ShellFailed as a valid output.
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

    let text = match output.exit_code {
        // Matches found.
        0 => truncate_output(&output.stdout),
        // Exit 1 = no matches (rg/grep spec). Partial output is possible when
        // some files matched and others did not (e.g. binary skip notices).
        1 if output.stdout.is_empty() => "No matches found.".to_string(),
        1 => truncate_output(&output.stdout),
        // Exit 2+ = tool error (invalid regex, permission denied, etc.).
        // Surface stderr so the LLM can diagnose the real problem.
        code => {
            let mut msg = format!("[grep error (exit {code})]");
            let stderr = output.stderr.trim();
            if !stderr.is_empty() {
                msg.push_str("\nstderr: ");
                msg.push_str(stderr);
            }
            msg
        }
    };

    Ok(ToolResult {
        content: vec![ContentBlock::Text { text }],
        details: Value::Null,
        terminate: false,
    })
}

/// Check whether ripgrep (`rg`) is available on PATH.
///
/// Probes by running `rg --version` rather than relying on `which`, which may
/// not be present in all environments (e.g. NixOS, minimal Docker images).
/// Result is cached for the lifetime of the process via [`OnceLock`] so the
/// subprocess is only spawned once.
fn has_ripgrep() -> bool {
    static RG_AVAILABLE: OnceLock<bool> = OnceLock::new();
    *RG_AVAILABLE.get_or_init(|| {
        std::process::Command::new("rg")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// Truncate output at MAX_OUTPUT_BYTES with a notice about omitted bytes.
/// Uses the same head+tail strategy as other tools.
fn truncate_output(output: &str) -> String {
    if output.len() <= MAX_OUTPUT_BYTES {
        return output.to_string();
    }

    // Keep head + tail: keep roughly half each.
    let half = MAX_OUTPUT_BYTES / 2;
    let head_end = output.floor_char_boundary(half);
    let tail_start = output.floor_char_boundary(output.len() - half);

    let omitted = output.len() - head_end - (output.len() - tail_start);
    format!(
        "{}[Output truncated: {} bytes omitted]\n{}",
        &output[..head_end],
        omitted,
        &output[tail_start..]
    )
}

/// Build a ripgrep command (rg).
fn build_rg_command(
    pattern: &str,
    path: &str,
    include: Option<&str>,
    context_lines: u64,
) -> String {
    let mut parts = vec![
        "rg".to_string(),
        "--color=never".to_string(),
        "-n".to_string(),
    ];

    if context_lines > 0 {
        parts.push(format!("-C{}", context_lines));
    }

    if let Some(glob) = include {
        parts.push(format!("--glob={}", shell_escape(glob)));
    }

    parts.push("--".to_string());
    parts.push(shell_escape(pattern));
    parts.push(shell_escape(path));
    parts.join(" ")
}

/// Build a standard grep command (fallback when rg is unavailable).
///
/// Uses `grep -rnI` (recursive, line numbers, no binary files).
/// Does NOT respect .gitignore (rg does this natively, grep does not).
fn build_grep_command(
    pattern: &str,
    path: &str,
    include: Option<&str>,
    context_lines: u64,
) -> String {
    let mut parts = vec![
        "grep".to_string(),
        "-rnI".to_string(),
        "--color=never".to_string(),
    ];

    if context_lines > 0 {
        parts.push(format!("-C{}", context_lines));
    }

    if let Some(glob) = include {
        parts.push(format!("--include={}", shell_escape(glob)));
    }

    parts.push("--".to_string());
    parts.push(shell_escape(pattern));
    parts.push(shell_escape(path));
    parts.join(" ")
}

fn shell_escape(s: &str) -> String {
    if cfg!(windows) {
        if s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '\\' | '/' | '.' | '_' | '-'))
        {
            s.to_string()
        } else {
            format!("\"{}\"", s.replace('"', "\\\""))
        }
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_helpers;
    use llm_harness_runtime_sandbox_os::OsEnv;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn build_rg_command_basic() {
        let cmd = build_rg_command("fn main", "src/", None, 0);
        assert!(cmd.starts_with("rg"), "got: {}", cmd);
        assert!(cmd.contains("fn main"), "got: {}", cmd);
        assert!(cmd.contains("src/"), "got: {}", cmd);
        assert!(cmd.contains("-n"), "should include line numbers");
        assert!(cmd.contains(" -- "), "should have -- separator: {}", cmd);
    }

    #[test]
    fn build_rg_command_with_context_and_include() {
        let cmd = build_rg_command("TODO", ".", Some("*.rs"), 2);
        assert!(cmd.contains("-C2"), "got: {}", cmd);
        assert!(cmd.contains("--glob="), "got: {}", cmd);
        assert!(cmd.contains("*.rs"), "got: {}", cmd);
        assert!(cmd.contains(" -- "), "should have -- separator: {}", cmd);
    }

    #[test]
    fn build_grep_command_basic() {
        let cmd = build_grep_command("fn main", "src/", None, 0);
        assert!(cmd.starts_with("grep"), "got: {}", cmd);
        assert!(cmd.contains("fn main"), "got: {}", cmd);
        assert!(cmd.contains("src/"), "got: {}", cmd);
        assert!(cmd.contains("-rnI"), "should include -rnI flags");
        assert!(cmd.contains(" -- "), "should have -- separator: {}", cmd);
    }

    #[test]
    fn build_grep_command_with_context_and_include() {
        let cmd = build_grep_command("TODO", ".", Some("*.rs"), 2);
        assert!(cmd.contains("-C2"), "got: {}", cmd);
        assert!(cmd.contains("--include="), "got: {}", cmd);
        assert!(cmd.contains("*.rs"), "got: {}", cmd);
        assert!(cmd.contains(" -- "), "should have -- separator: {}", cmd);
    }

    #[test]
    fn build_rg_command_dash_pattern_uses_separator() {
        // A pattern starting with '-' must come after '--' so rg doesn't
        // treat it as a flag.
        let cmd = build_rg_command("-v something", ".", None, 0);
        // '--' must appear before the pattern in the command string.
        let sep_pos = cmd.find(" -- ").expect("-- separator missing");
        let pat_pos = cmd.find("-v something").expect("pattern missing");
        assert!(sep_pos < pat_pos, "-- must precede pattern: {cmd}");
    }

    #[test]
    fn has_ripgrep_detects_rg_availability() {
        // Test that the function runs without error (result depends on env).
        let _available = has_ripgrep();
    }

    #[test]
    fn truncate_output_under_limit_unchanged() {
        let short = "hello\nworld\n";
        assert_eq!(truncate_output(short), short);
    }

    #[test]
    fn truncate_output_over_limit_has_head_and_tail() {
        let big = "a".repeat(MAX_OUTPUT_BYTES + 100);
        let result = truncate_output(&big);
        assert!(
            result.len() <= MAX_OUTPUT_BYTES + 100,
            "too long: {}",
            result.len()
        );
        assert!(result.contains("truncated"), "should have notice");
    }

    #[tokio::test]
    async fn grep_finds_pattern_in_file() {
        // Requires ripgrep (`rg`) to be installed.
        if !has_ripgrep() {
            return;
        }

        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello world\nfoo bar\n").unwrap();

        let env = Arc::new(OsEnv::new(dir.path().to_path_buf()));
        let ctx = test_helpers::make_ctx(env);

        let tool = GrepTool::new();
        let args = serde_json::json!({
            "pattern": "hello",
            "path": dir.path().to_str().unwrap()
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        let text = match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("hello"), "got: {:?}", text);
    }

    #[tokio::test]
    async fn grep_returns_no_matches_message() {
        // Requires ripgrep (`rg`) to be installed.
        if !has_ripgrep() {
            return;
        }

        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello world\n").unwrap();

        let env = Arc::new(OsEnv::new(dir.path().to_path_buf()));
        let ctx = test_helpers::make_ctx(env);

        let tool = GrepTool::new();
        let args = serde_json::json!({
            "pattern": "xyzzy_nonexistent_xyz",
            "path": dir.path().to_str().unwrap()
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        let text = match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("No matches"), "got: {:?}", text);
    }

    #[tokio::test]
    async fn grep_error_exit_surfaces_stderr() {
        // Requires ripgrep (`rg`) to be installed.
        if !has_ripgrep() {
            return;
        }

        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello\n").unwrap();

        let env = Arc::new(OsEnv::new(dir.path().to_path_buf()));
        let ctx = test_helpers::make_ctx(env);

        let tool = GrepTool::new();
        // An invalid regex causes rg to exit with code 2.
        let args = serde_json::json!({
            "pattern": "(?invalid",
            "path": dir.path().to_str().unwrap()
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        let text = match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        // Should report an error, not "No matches found."
        assert!(
            text.contains("grep error") || text.contains("error"),
            "expected error report, got: {:?}",
            text
        );
        assert!(
            !text.contains("No matches found."),
            "must not report 'No matches found.' for a tool error: {:?}",
            text
        );
    }

    #[test]
    fn grep_fallback_to_standard_grep() {
        // Verify that the grep command built by build_grep_command actually works.
        // Uses `grep` directly (no rg), so no async needed.
        if std::process::Command::new("grep")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }

        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello from grep").unwrap();

        // Manually run the grep command to verify it works.
        let cmd = build_grep_command("hello", dir.path().to_str().unwrap(), None, 0);
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .expect("grep should run");
        assert!(
            output.status.success(),
            "grep command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("hello"), "got: {:?}", stdout);
    }

    #[tokio::test]
    async fn grep_fallback_when_rg_not_found() {
        // Only run this test if rg is actually absent.
        // This test verifies the full execute path when falling back to grep.
        if has_ripgrep() {
            return;
        }

        if std::process::Command::new("grep")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }

        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello from grep fallback\n").unwrap();

        let env = Arc::new(OsEnv::new(dir.path().to_path_buf()));
        let ctx = test_helpers::make_ctx(env);

        let tool = GrepTool::new();
        let args = serde_json::json!({
            "pattern": "hello",
            "path": dir.path().to_str().unwrap()
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        let text = match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("hello"), "got: {:?}", text);
    }
}
