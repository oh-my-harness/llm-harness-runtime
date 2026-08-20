//! File discovery tool — glob pattern matching for file paths.

use std::path::PathBuf;

use futures::future::BoxFuture;
use glob::Pattern as GlobPattern;
use llm_harness_types::{
    DataBlock, EnvError, FileInfo, Tool, ToolContext, ToolError, ToolFailure, ToolResult,
};

use crate::file_walk::walk_dir_filtered;
use crate::resolve_path;
use crate::truncation::{TruncationConfig, TruncationOutcome, truncate_and_persist};

const DEFAULT_MAX_RESULTS: usize = 500;

/// Glob tool — find files by glob pattern.
///
/// Walks the directory tree under each given path, matching file paths
/// against a glob pattern. Results are deduplicated, sorted by modification
/// time (newest first), and truncated to `max_results`. The final output is
/// passed through `truncate_and_persist` for consistent output-size handling
/// with `BashTool` and `GrepTool`.
pub struct GlobTool;

impl GlobTool {
    /// Create a new `GlobTool`. The tool holds no state.
    pub fn new() -> Self {
        Self
    }
}

impl Default for GlobTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if a string contains glob metacharacters.
fn has_glob_chars(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{')
}

/// Parse a path entry into (base_path, glob_pattern).
///
/// - No glob chars: treat as directory, search with implicit `**/*`.
/// - Glob in first segment: base is `.`, auto-prefix `**/` if needed.
/// - Glob later in path: split at first glob-bearing segment.
///
/// Backslashes are normalized to forward slashes for glob matching.
/// This assumes Unix-style paths; on Windows the base path may not
/// resolve correctly if it contains a drive letter.
fn parse_glob_entry(entry: &str) -> (PathBuf, String) {
    if !has_glob_chars(entry) {
        // Directory path — implicit **/* search.
        // Strip trailing slash for a clean base path.
        let trimmed = entry.trim_end_matches('/');
        return (PathBuf::from(trimmed), "**/*".to_string());
    }

    // Find the first path segment containing glob chars.
    let normalized = entry.replace('\\', "/");
    let segments: Vec<&str> = normalized.split('/').collect();

    let mut base_parts: Vec<&str> = Vec::new();
    let mut glob_parts: Vec<&str> = Vec::new();
    let mut found_glob = false;

    for seg in &segments {
        if !found_glob && !has_glob_chars(seg) {
            base_parts.push(seg);
        } else {
            found_glob = true;
            glob_parts.push(seg);
        }
    }

    let base = if base_parts.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(base_parts.join("/"))
    };

    let pattern = glob_parts.join("/");
    // Auto-prefix **/ if the pattern doesn't already start with **.
    if pattern.starts_with("**/") || pattern.starts_with("**") {
        (base, pattern)
    } else {
        (base, format!("**/{pattern}"))
    }
}

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files by glob pattern, returning matching paths.\n\n\
         Use when: locating files by name or extension; listing all files of a type in a project; finding config files by pattern.\n\
         Don't use for: searching file contents (use grep); reading file contents (use read); executing commands (use bash).\n\
         Returns: Matching file paths, one per line. Sorted by modification time (newest first). Truncated to max_results.\n\n\
         Examples:\n\
         {\"paths\": [\"**/*.rs\"]}\n\
         {\"paths\": [\"src/**/*.ts\", \"tests/**/*.ts\"]}\n\
         {\"paths\": [\"config/*.json\"]}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "One or more glob patterns or directory paths. Bare patterns like \"*.rs\" are auto-prefixed with \"**/\". e.g. [\"**/*.rs\", \"src/**/*.ts\"]"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of matching paths to return (default 500). e.g. 50",
                        "default": 500
                    }
                },
                "required": ["paths"]
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
            let paths: Vec<String> = args
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            if paths.is_empty() {
                return Err(
                    ToolError::InvalidArguments("missing array field 'paths'".into()).into(),
                );
            }

            let max_results = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_RESULTS);

            if max_results == 0 {
                return Err(ToolError::InvalidArguments("max_results must be > 0".into()).into());
            }

            let working_dir = ctx.env.working_dir();
            let mut all_files: Vec<FileInfo> = Vec::new();
            let mut total_found = 0usize;
            let mut missing_paths: Vec<String> = Vec::new();

            for entry in &paths {
                let (base, pattern_str) = parse_glob_entry(entry);
                let resolved_base = resolve_path(working_dir, &base.to_string_lossy());

                let glob_pattern = match GlobPattern::new(&pattern_str) {
                    Ok(p) => p,
                    Err(e) => {
                        return Err(ToolError::InvalidArguments(format!(
                            "invalid glob pattern '{pattern_str}': {e}"
                        ))
                        .into());
                    }
                };

                // Closure: match file relative path against glob pattern.
                let wd = working_dir;
                let matcher = move |f: &FileInfo| -> bool {
                    let rel_path = f
                        .path
                        .strip_prefix(wd)
                        .unwrap_or(&f.path)
                        .to_string_lossy()
                        .to_string();
                    glob_pattern.matches(&rel_path)
                };

                // Check base path exists and is a directory.
                match ctx.env.file_info(&resolved_base, ctx.abort.clone()).await {
                    Ok(info) => {
                        if info.is_dir {
                            match walk_dir_filtered(
                                &*ctx.env,
                                &resolved_base,
                                &ctx.abort,
                                &matcher,
                                max_results,
                            )
                            .await
                            {
                                Ok((files, matched)) => {
                                    all_files.extend(files);
                                    total_found += matched;
                                }
                                Err(EnvError::Aborted) => return Err(ToolFailure::aborted()),
                                Err(_) => { /* skip unreadable directories */ }
                            }
                        } else {
                            // It's a file — check if it matches the pattern.
                            if matcher(&info) {
                                all_files.push(info);
                                total_found += 1;
                            }
                        }
                    }
                    Err(EnvError::NotFound(_)) => {
                        missing_paths.push(entry.clone());
                    }
                    Err(EnvError::Aborted) => return Err(ToolFailure::aborted()),
                    Err(_) => {
                        missing_paths.push(entry.clone());
                    }
                }
            }

            // Deduplicate by path.
            all_files.sort_by(|a, b| a.path.cmp(&b.path));
            all_files.dedup_by(|a, b| a.path == b.path);

            // Sort by modified time descending (newest first).
            all_files.sort_by_key(|f| std::cmp::Reverse(f.modified));

            // `total_found` counts all predicate-matching files (pre-dedup,
            // across all paths). When paths overlap, this may over-count;
            // `unique_count` is the post-dedup count actually returned.
            let unique_count = all_files.len();
            let truncated = total_found > max_results;
            if all_files.len() > max_results {
                all_files.truncate(max_results);
            }

            let mut text = if all_files.is_empty() {
                String::from("No files found matching pattern")
            } else {
                all_files
                    .iter()
                    .map(|f| {
                        f.path
                            .strip_prefix(working_dir)
                            .unwrap_or(&f.path)
                            .to_string_lossy()
                            .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };

            if truncated {
                text.push_str(&format!(
                    "\n\n({} file{} found, showing first {})",
                    unique_count,
                    if unique_count == 1 { "" } else { "s" },
                    all_files.len()
                ));
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
                    "file_count": all_files.len(),
                    "total_found": total_found,
                    "truncated": outcome.truncated || truncated,
                }),
                false,
            ))
        })
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
            tool_use_id: "tu_glob".into(),
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
    async fn simple_pattern() {
        let env = Arc::new(MockExecutionEnv::new());
        env.mkdir_sync(&PathBuf::from("src"));
        env.mkdir_sync(&PathBuf::from("src/utils"));
        env.write_file_sync(&PathBuf::from("src/main.rs"), "fn main() {}\n");
        env.write_file_sync(&PathBuf::from("src/utils/helper.rs"), "fn helper() {}\n");
        env.write_file_sync(&PathBuf::from("src/readme.txt"), "readme\n");

        let tool = GlobTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "paths": ["**/*.rs"]
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        let text = text.replace('\\', "/");
        assert!(text.contains("src/main.rs"));
        assert!(text.contains("src/utils/helper.rs"));
        assert!(!text.contains("readme.txt"));
    }

    #[tokio::test]
    async fn auto_prefix_bare_glob() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("main.rs"), "fn main() {}\n");
        env.write_file_sync(&PathBuf::from("other.txt"), "other\n");

        let tool = GlobTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "paths": ["*.rs"]
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("main.rs"));
        assert!(!text.contains("other.txt"));
    }

    #[tokio::test]
    async fn directory_path_implicit_recursive() {
        let env = Arc::new(MockExecutionEnv::new());
        env.mkdir_sync(&PathBuf::from("src"));
        env.mkdir_sync(&PathBuf::from("src/sub"));
        env.write_file_sync(&PathBuf::from("src/a.rs"), "a\n");
        env.write_file_sync(&PathBuf::from("src/sub/b.rs"), "b\n");

        let tool = GlobTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "paths": ["src/"]
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        let text = text.replace('\\', "/");
        assert!(text.contains("src/a.rs"));
        assert!(text.contains("src/sub/b.rs"));
    }

    #[tokio::test]
    async fn multiple_paths_dedup() {
        let env = Arc::new(MockExecutionEnv::new());
        env.mkdir_sync(&PathBuf::from("src"));
        env.write_file_sync(&PathBuf::from("src/shared.rs"), "shared\n");

        let tool = GlobTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "paths": ["src/", "src/**/*.rs"]
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let details = &result.details;
        assert_eq!(details["file_count"].as_u64().unwrap(), 1);
    }

    #[tokio::test]
    async fn max_results_truncation() {
        let env = Arc::new(MockExecutionEnv::new());
        env.mkdir_sync(&PathBuf::from("files"));
        for i in 0..20 {
            env.write_file_sync(&PathBuf::from(format!("files/file{i}.rs")), "content\n");
        }

        let tool = GlobTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "paths": ["files/**/*.rs"],
            "max_results": 5
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let details = &result.details;
        assert_eq!(details["file_count"].as_u64().unwrap(), 5);
        assert_eq!(details["total_found"].as_u64().unwrap(), 20);
        assert!(details["truncated"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn no_matches() {
        let env = Arc::new(MockExecutionEnv::new());
        env.mkdir_sync(&PathBuf::from("src"));
        env.write_file_sync(&PathBuf::from("src/main.rs"), "fn main() {}\n");

        let tool = GlobTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "paths": ["**/*.py"]
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("No files found"));
    }

    #[tokio::test]
    async fn missing_path_skipped() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("real.rs"), "content\n");

        let tool = GlobTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "paths": ["nonexistent/*.rs", "**/*.rs"]
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("real.rs"));
        assert!(text.contains("Skipped missing paths"));
    }

    #[tokio::test]
    async fn max_results_zero_rejected() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("main.rs"), "fn main() {}\n");

        let tool = GlobTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "paths": ["**/*.rs"],
            "max_results": 0
        });
        let result = tool.execute(args, &ctx).await;
        assert!(result.is_err());
    }

    #[test]
    fn parse_glob_entry_no_glob() {
        let (base, pattern) = parse_glob_entry("src/");
        assert_eq!(base, PathBuf::from("src"));
        assert_eq!(pattern, "**/*");
    }

    #[test]
    fn parse_glob_entry_bare_glob() {
        let (base, pattern) = parse_glob_entry("*.rs");
        assert_eq!(base, PathBuf::from("."));
        assert_eq!(pattern, "**/*.rs");
    }

    #[test]
    fn parse_glob_entry_path_glob() {
        let (base, pattern) = parse_glob_entry("src/**/*.rs");
        assert_eq!(base, PathBuf::from("src"));
        assert_eq!(pattern, "**/*.rs");
    }

    #[tokio::test]
    async fn max_results_not_truncated_when_exact() {
        // When exactly max_results files match, truncated should be false.
        let env = Arc::new(MockExecutionEnv::new());
        env.mkdir_sync(&PathBuf::from("files"));
        for i in 0..5 {
            env.write_file_sync(&PathBuf::from(format!("files/file{i}.rs")), "content\n");
        }

        let tool = GlobTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "paths": ["files/**/*.rs"],
            "max_results": 5
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let details = &result.details;
        assert_eq!(details["file_count"].as_u64().unwrap(), 5);
        assert_eq!(details["total_found"].as_u64().unwrap(), 5);
        assert!(
            !details["truncated"].as_bool().unwrap(),
            "should not be truncated when exact match"
        );
    }

    #[tokio::test]
    async fn max_results_truncated_when_exceeding() {
        // When more than max_results files match, truncated should be true.
        let env = Arc::new(MockExecutionEnv::new());
        env.mkdir_sync(&PathBuf::from("files"));
        for i in 0..10 {
            env.write_file_sync(&PathBuf::from(format!("files/file{i}.rs")), "content\n");
        }

        let tool = GlobTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "paths": ["files/**/*.rs"],
            "max_results": 5
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let details = &result.details;
        assert_eq!(details["file_count"].as_u64().unwrap(), 5);
        assert_eq!(details["total_found"].as_u64().unwrap(), 10);
        assert!(
            details["truncated"].as_bool().unwrap(),
            "should be truncated when exceeding"
        );
    }

    #[tokio::test]
    async fn file_matching_pattern_directly() {
        // A file path (not directory) that matches the glob pattern.
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("main.rs"), "fn main() {}\n");
        env.write_file_sync(&PathBuf::from("other.txt"), "other\n");

        let tool = GlobTool::new();
        let ctx = make_ctx(env);
        let args = serde_json::json!({
            "paths": ["main.rs"]
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("main.rs"));
        assert!(!text.contains("other.txt"));
    }
}
