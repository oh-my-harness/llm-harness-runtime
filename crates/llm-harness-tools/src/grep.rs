//! Content search tool — regex matching across files and directories.

use std::borrow::Cow;

use futures::future::BoxFuture;
use glob::Pattern as GlobPattern;
use llm_harness_types::{
    DataBlock, EnvError, FileInfo, Tool, ToolContext, ToolError, ToolFailure, ToolResult,
};
use regex::RegexBuilder;

use crate::file_walk::walk_dir_filtered;
use crate::resolve_path;
use crate::truncation::{TruncationConfig, TruncationOutcome, truncate_and_persist};

const DEFAULT_MAX_RESULTS: usize = 200;
const MAX_LINE_LENGTH: usize = 512;
const MAX_FILE_SIZE_BYTES: u64 = 4 * 1024 * 1024;
/// Cap on context lines (before/after) to prevent unbounded memory growth
/// on large files with many matches.
const MAX_CONTEXT_LINES: usize = 1000;
/// Maximum number of candidate files to search. Walk stops early once this
/// many files have been collected, preventing unbounded memory usage.
const MAX_FILES: usize = 1000;

/// Grep tool — search file contents for a regex pattern.
///
/// Walks the directory tree under each given path, reads each candidate
/// file, and collects lines matching the regex. Supports context lines
/// (before/after), filename include filter, case sensitivity, and result
/// truncation. The final output is passed through `truncate_and_persist`
/// for consistent output-size handling with `BashTool`.
pub struct GrepTool;

impl GrepTool {
    /// Create a new `GrepTool`. The tool holds no state.
    pub fn new() -> Self {
        Self
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
        "Search file contents for a regex pattern across one or more files or directories.\n\n\
         Use when: finding where a function/string is used in code; searching for TODO/FIXME markers; locating config values across files.\n\
         Don't use for: finding files by name (use glob); running one-off shell commands (use bash); reading a specific file (use read).\n\
         Returns: Matching lines formatted as file_path:line_number:content. Context lines are prefixed with a space. Truncated to max_results; long lines truncated to 512 chars.\n\n\
         Examples:\n\
         {\"pattern\": \"fn main\\\\(\", \"paths\": [\"src/main.rs\"]}\n\
         {\"pattern\": \"TODO|FIXME\", \"paths\": [\"src/\"]}\n\
         {\"pattern\": \"println!\", \"paths\": [\"src/\", \"tests/\"], \"include\": \"*.rs\", \"context_after\": 3}\n\
         {\"pattern\": \"password\", \"paths\": [\"config/\"], \"case_sensitive\": false, \"max_results\": 10}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern (Rust regex syntax, RE2-style; no lookaround or backreferences). e.g. \"fn main\\\\(\" or \"TODO|FIXME\""
                    },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "One or more files or directories to search. Directories are searched recursively. Defaults to [\".\"] if omitted. e.g. [\"src/\", \"tests/main.rs\"]"
                    },
                    "include": {
                        "type": "string",
                        "description": "Optional glob filter matched against file names only (not paths). e.g. \"*.rs\""
                    },
                    "case_sensitive": {
                        "type": "boolean",
                        "description": "Whether to match case-sensitively (default true). e.g. false",
                        "default": true
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum matching lines to return (default 200). e.g. 50",
                        "default": 200
                    },
                    "context_before": {
                        "type": "integer",
                        "description": "Context lines to show before each match (default 0). e.g. 3",
                        "default": 0
                    },
                    "context_after": {
                        "type": "integer",
                        "description": "Context lines to show after each match (default 0). e.g. 3",
                        "default": 0
                    }
                },
                "required": ["pattern"]
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
            let pattern_str = args
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidArguments("missing string field 'pattern'".into())
                })?;

            let case_sensitive = args
                .get("case_sensitive")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            let max_results = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_RESULTS);

            if max_results == 0 {
                return Err(ToolError::InvalidArguments("max_results must be > 0".into()).into());
            }

            let context_before = args
                .get("context_before")
                .and_then(|v| v.as_u64())
                .map(|n| (n as usize).min(MAX_CONTEXT_LINES))
                .unwrap_or(0);

            let context_after = args
                .get("context_after")
                .and_then(|v| v.as_u64())
                .map(|n| (n as usize).min(MAX_CONTEXT_LINES))
                .unwrap_or(0);

            let include_filter = match args.get("include").and_then(|v| v.as_str()) {
                Some(s) => match GlobPattern::new(s) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        return Err(ToolError::InvalidArguments(format!(
                            "invalid include pattern '{s}': {e}"
                        ))
                        .into());
                    }
                },
                None => None,
            };

            let paths: Vec<String> = args
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_else(|| vec![".".to_string()]);

            if paths.is_empty() {
                return Err(ToolError::InvalidArguments("paths must not be empty".into()).into());
            }

            let regex = RegexBuilder::new(pattern_str)
                .case_insensitive(!case_sensitive)
                .build()
                .map_err(|e| ToolError::InvalidArguments(format!("invalid regex: {e}")))?;

            let working_dir = ctx.env.working_dir();
            let mut all_files: Vec<FileInfo> = Vec::new();
            let mut missing_paths: Vec<String> = Vec::new();

            // Combined size + include filter, applied during walk so the
            // MAX_FILES cap counts only relevant candidates.
            let matches_filter = |f: &FileInfo| {
                f.size <= MAX_FILE_SIZE_BYTES
                    && match &include_filter {
                        Some(filter) => f
                            .path
                            .file_name()
                            .map(|name| filter.matches(name.to_string_lossy().as_ref()))
                            .unwrap_or(false),
                        None => true,
                    }
            };
            for path_str in &paths {
                if all_files.len() >= MAX_FILES {
                    break;
                }
                let resolved = resolve_path(working_dir, path_str);
                match ctx.env.file_info(&resolved, ctx.abort.clone()).await {
                    Ok(info) => {
                        if info.is_dir {
                            let remaining = MAX_FILES - all_files.len();
                            match walk_dir_filtered(
                                &*ctx.env,
                                &resolved,
                                &ctx.abort,
                                &matches_filter,
                                remaining,
                            )
                            .await
                            {
                                Ok((files, _)) => all_files.extend(files),
                                Err(EnvError::Aborted) => return Err(ToolFailure::aborted()),
                                Err(_) => { /* skip unreadable directories */ }
                            }
                        } else if matches_filter(&info) {
                            all_files.push(info);
                        }
                    }
                    Err(EnvError::NotFound(_)) => {
                        missing_paths.push(path_str.clone());
                    }
                    Err(EnvError::Aborted) => return Err(ToolFailure::aborted()),
                    Err(_) => {
                        missing_paths.push(path_str.clone());
                    }
                }
            }

            if all_files.is_empty() {
                let mut text = String::from("No matches found");
                if !missing_paths.is_empty() {
                    text.push_str(&format!(
                        "\n\nSkipped missing paths: {}",
                        missing_paths.join(", ")
                    ));
                }
                return Ok(ToolResult::full(
                    vec![DataBlock::text(text)],
                    serde_json::json!({ "match_count": 0, "matched_file_count": 0, "truncated": false }),
                    false,
                ));
            }

            // Deduplicate by path.
            all_files.sort_by(|a, b| a.path.cmp(&b.path));
            all_files.dedup_by(|a, b| a.path == b.path);

            let mut output_lines: Vec<String> = Vec::new();
            let mut match_count = 0usize;
            let mut file_count = 0usize;
            let mut truncated = false;

            for file in &all_files {
                if match_count >= max_results {
                    truncated = true;
                    break;
                }

                let lines = match ctx
                    .env
                    .read_text_lines(&file.path, None, ctx.abort.clone())
                    .await
                {
                    Ok(lines) => lines,
                    Err(EnvError::InvalidUtf8(_)) => continue, // skip binary
                    Err(EnvError::Aborted) => return Err(ToolFailure::aborted()),
                    Err(_) => {
                        continue;
                    }
                };

                let file_path_str = file
                    .path
                    .strip_prefix(working_dir)
                    .unwrap_or(&file.path)
                    .to_string_lossy()
                    .to_string();

                let mut file_had_match = false;
                let total = lines.len();
                // Highest line index already emitted for this file (0-based).
                // Prevents duplicate context lines when matches overlap.
                let mut last_emitted: Option<usize> = None;

                // Precompute match flags to avoid redundant regex evaluation
                // in the context-after loop.
                let match_flags: Vec<bool> = lines.iter().map(|l| regex.is_match(l)).collect();

                for (i, line) in lines.iter().enumerate() {
                    if match_flags[i] {
                        if match_count >= max_results {
                            truncated = true;
                            break;
                        }

                        if !file_had_match {
                            file_had_match = true;
                            file_count += 1;
                        }

                        // Context before — skip lines already emitted.
                        let start = i.saturating_sub(context_before);
                        for (ctx_i, ctx_line) in lines[start..i].iter().enumerate() {
                            let line_no = start + ctx_i;
                            if last_emitted.is_some_and(|last| line_no <= last) {
                                continue;
                            }
                            output_lines.push(format!(
                                " {}:{}:{}",
                                file_path_str,
                                line_no + 1,
                                truncate_line(ctx_line)
                            ));
                        }

                        // Match line.
                        let match_line = truncate_line(line);
                        output_lines.push(format!("{}:{}:{}", file_path_str, i + 1, match_line));
                        match_count += 1;
                        last_emitted = Some(i);

                        // Context after — skip lines already emitted or lines that
                        // match the regex (they'll get their own match entry).
                        let end = i.saturating_add(1).saturating_add(context_after).min(total);
                        for (ctx_off, ctx_line) in lines[i + 1..end].iter().enumerate() {
                            let line_no = i + 1 + ctx_off;
                            if last_emitted.is_some_and(|last| line_no <= last)
                                || match_flags[line_no]
                            {
                                continue;
                            }
                            output_lines.push(format!(
                                " {}:{}:{}",
                                file_path_str,
                                line_no + 1,
                                truncate_line(ctx_line)
                            ));
                            last_emitted = Some(line_no);
                        }
                    }
                }
            }

            let mut text = output_lines.join("\n");
            if text.is_empty() {
                text = "No matches found".to_string();
            }

            // Summary line.
            if match_count > 0 {
                text.push_str(&format!(
                    "\n\n({} match{} in {} file{})",
                    match_count,
                    if match_count == 1 { "" } else { "es" },
                    file_count,
                    if file_count == 1 { "" } else { "s" }
                ));
                if truncated {
                    text.push_str(&format!(", showing first {}", match_count));
                }
            }

            if !missing_paths.is_empty() {
                text.push_str(&format!(
                    "\n\nSkipped missing paths: {}",
                    missing_paths.join(", ")
                ));
            }

            // Final truncation through truncate_and_persist.
            let outcome = truncate_and_persist(&text, ctx, &TruncationConfig::default())
                .await
                .unwrap_or_else(|_| TruncationOutcome {
                    output: text.clone(),
                    truncated: false,
                });

            Ok(ToolResult::full(
                vec![DataBlock::text(outcome.output)],
                serde_json::json!({
                    "match_count": match_count,
                    "matched_file_count": file_count,
                    "truncated": outcome.truncated || truncated,
                }),
                false,
            ))
        })
    }
}

/// Truncate a line to `MAX_LINE_LENGTH` chars, appending `…` if truncated.
///
/// Returns a borrowed `&str` when the line is within the limit (no
/// allocation), and an owned `String` only when truncation is needed.
fn truncate_line(line: &str) -> Cow<'_, str> {
    if line.chars().count() <= MAX_LINE_LENGTH {
        Cow::Borrowed(line)
    } else {
        let truncated: String = line.chars().take(MAX_LINE_LENGTH).collect();
        Cow::Owned(format!("{truncated}…"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::MockExecutionEnv;
    use chrono::Utc;
    use llm_harness_types::{
        AssistantMessage, AssistantMessageKind, RunContext, RunRequest, ToolContext,
    };
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Build a `ToolContext` backed by `env` with a no-op update channel.
    fn make_ctx(env: Arc<MockExecutionEnv>) -> ToolContext {
        let (tx, _rx) = mpsc::channel(1);
        ToolContext {
            run: Arc::new(RunContext::new(RunRequest::default())),
            env,
            abort: CancellationToken::new(),
            tool_use_id: "tu_grep".into(),
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
    async fn single_file_match() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(
            &PathBuf::from("src/main.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        );

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "println!",
            "paths": ["src/main.rs"]
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("src/main.rs:2:"));
        assert!(text.contains("println!"));
    }

    #[tokio::test]
    async fn directory_recursive() {
        let env = Arc::new(MockExecutionEnv::new());
        env.mkdir_sync(&PathBuf::from("src"));
        env.mkdir_sync(&PathBuf::from("src/utils"));
        env.write_file_sync(&PathBuf::from("src/main.rs"), "fn main() {}\n");
        env.write_file_sync(
            &PathBuf::from("src/utils/helper.rs"),
            "fn helper() {}\nfn main() {}\n",
        );

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "fn main",
            "paths": ["src/"]
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("src/main.rs:1:"));
        assert!(text.contains("src/utils/helper.rs:2:"));
    }

    #[tokio::test]
    async fn include_filter() {
        let env = Arc::new(MockExecutionEnv::new());
        env.mkdir_sync(&PathBuf::from("src"));
        env.write_file_sync(&PathBuf::from("src/main.rs"), "println!(\"rs\");\n");
        env.write_file_sync(&PathBuf::from("src/main.txt"), "println!(\"txt\");\n");

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "println!",
            "paths": ["src/"],
            "include": "*.rs"
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("main.rs"));
        assert!(!text.contains("main.txt"));
    }

    #[tokio::test]
    async fn case_insensitive() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(
            &PathBuf::from("config.txt"),
            "PASSWORD=secret\nPassword=other\n",
        );

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "password",
            "paths": ["config.txt"],
            "case_sensitive": false
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("config.txt:1:"));
        assert!(text.contains("config.txt:2:"));
    }

    #[tokio::test]
    async fn context_lines() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(
            &PathBuf::from("code.rs"),
            "line1\nline2\nMATCH\nline4\nline5\n",
        );

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "MATCH",
            "paths": ["code.rs"],
            "context_before": 1,
            "context_after": 1
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        // Match line at line 3.
        assert!(text.contains("code.rs:3:MATCH"));
        // Context before (line 2) — prefixed with space.
        assert!(text.contains(" code.rs:2:line2"));
        // Context after (line 4) — prefixed with space.
        assert!(text.contains(" code.rs:4:line4"));
    }

    #[tokio::test]
    async fn max_results_truncation() {
        let env = Arc::new(MockExecutionEnv::new());
        let mut content = String::new();
        for i in 0..50 {
            content.push_str(&format!("MATCH line {}\n", i));
        }
        env.write_file_sync(&PathBuf::from("big.rs"), &content);

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "MATCH",
            "paths": ["big.rs"],
            "max_results": 10
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let details = &result.details;
        assert_eq!(details["match_count"].as_u64().unwrap(), 10);
        assert!(details["truncated"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn no_matches() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("empty.rs"), "fn foo() {}\n");

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "nonexistent_pattern_xyz",
            "paths": ["empty.rs"]
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("No matches found"));
    }

    #[tokio::test]
    async fn missing_path_skipped() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("real.rs"), "fn main() {}\n");

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "fn main",
            "paths": ["real.rs", "nonexistent.rs"]
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("real.rs:1:"));
        assert!(text.contains("Skipped missing paths"));
        assert!(text.contains("nonexistent.rs"));
    }

    #[tokio::test]
    async fn multiple_paths_dedup() {
        let env = Arc::new(MockExecutionEnv::new());
        env.mkdir_sync(&PathBuf::from("src"));
        env.write_file_sync(&PathBuf::from("src/shared.rs"), "fn shared() {}\n");

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "fn shared",
            "paths": ["src/", "src/shared.rs"]
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let details = &result.details;
        // Deduplication: should only count 1 match, not 2.
        assert_eq!(details["match_count"].as_u64().unwrap(), 1);
    }

    #[tokio::test]
    async fn default_paths_to_dot() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("main.rs"), "fn main() {}\n");

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "fn main"
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("main.rs:1:"));
    }

    #[tokio::test]
    async fn overlapping_context_no_duplicates() {
        let env = Arc::new(MockExecutionEnv::new());
        // Adjacent matches: lines 2 and 3 both match.
        // With context_before=1, context_after=1:
        //   match@2 → context-before: line1, match: line2, context-after: line3
        //   match@3 → context-before: line2 (already emitted), match: line3, context-after: line4
        // Line 2 and line 3 should each appear exactly once as a match line,
        // and no context line should be duplicated.
        env.write_file_sync(&PathBuf::from("code.rs"), "line1\nMATCH\nMATCH\nline4\n");

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "MATCH",
            "paths": ["code.rs"],
            "context_before": 1,
            "context_after": 1
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };

        // Count occurrences of each line — context lines are prefixed with " ".
        // line1 (context for match@2): exactly 1
        assert_eq!(text.matches(" code.rs:1:line1").count(), 1);
        // line2 (match): exactly 1
        assert_eq!(text.matches("code.rs:2:MATCH").count(), 1);
        // line3 (match): exactly 1, NOT duplicated as context for match@2
        assert_eq!(text.matches("code.rs:3:MATCH").count(), 1);
        // line4 (context for match@3): exactly 1
        assert_eq!(text.matches(" code.rs:4:line4").count(), 1);
    }

    #[tokio::test]
    async fn max_results_zero_rejected() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("main.rs"), "fn main() {}\n");

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "fn main",
            "paths": ["main.rs"],
            "max_results": 0
        });
        let result = tool.execute(args, &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn invalid_include_pattern_rejected() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("main.rs"), "fn main() {}\n");

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "fn main",
            "paths": ["main.rs"],
            "include": "["
        });
        let result = tool.execute(args, &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn context_before_multiple_lines_ascending() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(
            &PathBuf::from("code.rs"),
            "line1\nline2\nline3\nMATCH\nline5\n",
        );

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "MATCH",
            "paths": ["code.rs"],
            "context_before": 2
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        // Context-before must be in ascending order: line2 before line3.
        let pos2 = text.find(" code.rs:2:line2").unwrap();
        let pos3 = text.find(" code.rs:3:line3").unwrap();
        assert!(pos2 < pos3, "context-before lines must be ascending");
        assert!(text.contains("code.rs:4:MATCH"));
    }

    #[tokio::test]
    async fn context_after_multiple_lines() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(
            &PathBuf::from("code.rs"),
            "line1\nMATCH\nline3\nline4\nline5\n",
        );

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "MATCH",
            "paths": ["code.rs"],
            "context_after": 2
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        // Context-after lines in ascending order.
        let pos3 = text.find(" code.rs:3:line3").unwrap();
        let pos4 = text.find(" code.rs:4:line4").unwrap();
        assert!(pos3 < pos4, "context-after lines must be ascending");
        assert!(!text.contains("line5"), "line5 should not be included");
    }

    #[tokio::test]
    async fn max_files_cap_across_paths() {
        let env = Arc::new(MockExecutionEnv::new());
        env.mkdir_sync(&PathBuf::from("dir1"));
        env.mkdir_sync(&PathBuf::from("dir2"));
        // Create more than MAX_FILES files across two directories.
        for i in 0..600 {
            env.write_file_sync(&PathBuf::from(format!("dir1/f{i}.rs")), "content\n");
        }
        for i in 0..600 {
            env.write_file_sync(&PathBuf::from(format!("dir2/f{i}.rs")), "content\n");
        }

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "nonexistent_xyz_unique",
            "paths": ["dir1/", "dir2/"]
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        // Should not panic or exceed MAX_FILES.
        let _text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
    }

    #[tokio::test]
    async fn context_lines_clamped_to_max() {
        // context_before beyond MAX_CONTEXT_LINES should be clamped, not
        // cause unbounded memory allocation.
        let env = Arc::new(MockExecutionEnv::new());
        let mut content = String::new();
        for i in 0..2000 {
            content.push_str(&format!("line{i}\n"));
        }
        content.push_str("MATCH\n");
        env.write_file_sync(&PathBuf::from("big.txt"), &content);

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "MATCH",
            "paths": ["big.txt"],
            "context_before": 999999999
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        // Should contain the match and at most MAX_CONTEXT_LINES context lines.
        assert!(text.contains("MATCH"));
        // With 2001 lines and context clamped to 1000, line "line1000" should
        // be the earliest context line (index 1001, 0-based). Line "line0"
        // should NOT appear.
        assert!(!text.contains("big.txt:1:"), "line 1 should not appear");
    }

    #[tokio::test]
    async fn include_filter_applied_during_walk() {
        // Non-matching files should not consume the MAX_FILES cap when an
        // include filter is specified.
        let env = Arc::new(MockExecutionEnv::new());
        env.mkdir_sync(&PathBuf::from("dir"));
        // Fill with .txt files that would exceed MAX_FILES if not filtered
        // during walk.
        for i in 0..1100 {
            env.write_file_sync(
                &PathBuf::from(format!("dir/filler{i}.txt")),
                "no match here\n",
            );
        }
        // This .rs file comes after the .txt files alphabetically but should
        // still be found because the include filter excludes .txt during walk.
        env.write_file_sync(&PathBuf::from("dir/target.rs"), "println!(\"found\");\n");

        let tool = GrepTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "pattern": "println!",
            "paths": ["dir/"],
            "include": "*.rs"
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(
            text.contains("target.rs"),
            "target.rs should be found despite 1100 .txt files: {text}"
        );
    }
}
