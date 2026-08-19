//! The `read` tool: read a file (with hashline anchoring when a snapshot
//! store is attached) or list a directory.
//!
//! In hashline mode the output is prefixed with a `[PATH#TAG]` header so a
//! subsequent `edit` can detect stale content via the [`FileSnapshotStore`].
//! `raw` mode emits plain text with no header or line numbers.

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolError, ToolFailure, ToolResult};
use parking_lot::RwLock;

use crate::resolve_path;
use crate::snapshot::FileSnapshotStore;

/// Default number of lines returned when no `offset`/`limit` is given.
const DEFAULT_MAX_LINES: usize = 300;

/// Extra lines included above an explicit `offset`/`limit` range.
const RANGE_LEADING_CONTEXT_LINES: usize = 1;

/// Extra lines included below an explicit `offset`/`limit` range.
const RANGE_TRAILING_CONTEXT_LINES: usize = 3;

/// Upper bound on the number of bytes read into memory for a single `read`
/// call. Files larger than this are NOT read in full — instead only the
/// requested window (plus context lines) is read via `read_text_lines(path,
/// Some(max_lines))`, matching spec point 4. This prevents OOM on multi-GiB
/// files. Set to match `FileSnapshotStore`'s storage cap: a file small enough
/// to snapshot is small enough to read in full for anchoring.
const READ_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Maximum directory entries listed before truncation.
const DIR_MAX_ENTRIES: usize = 12;

/// `read` tool — read a file or list a directory.
///
/// When a [`FileSnapshotStore`] is attached and `raw` is not requested, the
/// full file content is recorded and the output is prefixed with a
/// `[PATH#TAG]` header carrying the 4-hex content-hash tag, so a later
/// `edit` against the returned lines can detect stale content.
pub struct ReadTool {
    store: Option<Arc<RwLock<FileSnapshotStore>>>,
}

impl ReadTool {
    /// Create a `ReadTool`. Pass `Some(store)` to enable hashline anchoring;
    /// pass `None` for plain numbered output.
    pub fn new(store: Option<Arc<RwLock<FileSnapshotStore>>>) -> Self {
        Self { store }
    }
}

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file's contents with line numbers, or list a directory's entries.\n\n\
         Use when: inspecting source code, config files, or any text file; browsing directory structure; reading a range of lines from a large file.\n\
         Don't use for: executing commands (use bash); writing or modifying files (use write/edit); searching file contents (use grep).\n\
         Returns: Line-numbered file content with a header tag for safe editing. Directory reads return entry names with sizes and timestamps. Range reads include 1 leading and 3 trailing context lines.\n\n\
         Examples:\n\
         {\"path\": \"src/main.rs\"}\n\
         {\"path\": \"./tests/\"}\n\
         {\"path\": \"src/main.rs\", \"offset\": 50, \"limit\": 100}\n\
         {\"path\": \"config.json\", \"raw\": true}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file or directory (relative or absolute). e.g. \"src/main.rs\" or \"./tests/\""
                    },
                    "offset": {
                        "type": "integer",
                        "description": "1-indexed starting line for range reads. e.g. 50 (starts at line 50)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to return. e.g. 100"
                    },
                    "raw": {
                        "type": "boolean",
                        "description": "If true, return plain text without headers or line numbers. e.g. false",
                        "default": false
                    }
                },
                "required": ["path"]
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
            let path_str = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ToolError::InvalidArguments("missing string field 'path'".into()))?;
            let offset = args
                .get("offset")
                .and_then(serde_json::Value::as_u64)
                .map(|n| n as usize);
            let limit = args
                .get("limit")
                .and_then(serde_json::Value::as_u64)
                .map(|n| n as usize);
            let raw = args
                .get("raw")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);

            let working_dir = ctx.env.working_dir();
            let path = resolve_path(working_dir, path_str);

            let info = ctx
                .env
                .file_info(&path, ctx.abort.clone())
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;

            let (text, details) = if info.is_dir {
                let body = read_directory(&path, ctx).await?;
                (body, serde_json::json!({ "path": path, "is_dir": true }))
            } else {
                let (body, meta) =
                    read_file(&path, offset, limit, raw, info.size, &self.store, ctx).await?;
                (body, meta)
            };

            Ok(ToolResult::full(
                vec![DataBlock::text(text)],
                details,
                false,
            ))
        })
    }
}

/// Read a file and format its output according to `raw` / store presence.
///
/// `size` is the file size in bytes (from `file_info`); it gates the
/// full-read vs. windowed-read path to avoid OOM on large files (P1-3).
async fn read_file(
    path: &std::path::Path,
    offset: Option<usize>,
    limit: Option<usize>,
    raw: bool,
    size: u64,
    store: &Option<Arc<RwLock<FileSnapshotStore>>>,
    ctx: &ToolContext,
) -> Result<(String, serde_json::Value), ToolError> {
    let effective_limit = limit.unwrap_or(DEFAULT_MAX_LINES);
    let requested_offset = offset.unwrap_or(1);
    // 0-indexed start (un-clamped; clamped per-path below).
    let start = requested_offset.saturating_sub(1);

    // Large-file path: avoid reading the whole file into memory. Read only
    // the requested window (plus context lines) by passing `max_lines` to the
    // env, and skip snapshot recording (hashline anchoring is unavailable for
    // files beyond the snapshot cap anyway).
    if size > READ_MAX_BYTES {
        return read_file_windowed(path, start, effective_limit, raw, offset, limit, ctx).await;
    }

    // Small-file path: read the whole file so we can snapshot it, report
    // `total_lines`, and detect out-of-bounds requests.
    let all_lines = match ctx.env.read_text_lines(path, None, ctx.abort.clone()).await {
        Ok(lines) => lines,
        Err(llm_harness_types::EnvError::InvalidUtf8(_)) => {
            let bytes = ctx
                .env
                .read_binary_file(path, ctx.abort.clone())
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;
            let n = bytes.len();
            return Ok((
                format!("[Binary file, {n} bytes]"),
                serde_json::json!({ "path": path, "bytes": n, "binary": true }),
            ));
        }
        Err(e) => return Err(ToolError::Execution(e.to_string())),
    };

    // Note: an empty (0-byte) file yields `Ok(vec![])` from `read_text_lines`,
    // which correctly renders as an empty body below — no binary handling
    // needed. A file with bytes but no newlines yields a single line, also
    // handled by the normal path.

    let total = all_lines.len();
    // Clamp the requested start into the valid 1-indexed range [1, total+1].
    let start = start.min(total);
    let end = (start + effective_limit).min(total);

    let (display_start, display_end) = if offset.is_some() || limit.is_some() {
        let ds = start.saturating_sub(RANGE_LEADING_CONTEXT_LINES);
        let de = (end + RANGE_TRAILING_CONTEXT_LINES).min(total);
        (ds, de)
    } else {
        (start, end)
    };

    let display_start = display_start.min(display_end);
    let sliced: Vec<&String> = all_lines[display_start..display_end].iter().collect();

    // Detect out-of-bounds requests so we can append a guidance hint.
    // Two cases: offset past end of file, or limit pushed past the end.
    let oob = (offset.is_some() && requested_offset > total)
        || (limit.is_some() && start + effective_limit > total);
    let suggestion = if oob {
        let last = total.saturating_sub(1).max(1);
        if requested_offset > total {
            Some(format!(
                "\n(out of bounds: offset {requested_offset} exceeds {total} lines; use :1 to start from the beginning or :{last} for the last line)"
            ))
        } else {
            Some(format!("\n(use :{last} to read the last line)"))
        }
    } else {
        None
    };

    // Record a snapshot of the full file unless raw mode was requested.
    let tag_opt = if !raw {
        store
            .as_ref()
            .map(|s| s.read().record(path, all_lines.clone()))
    } else {
        None
    };

    let mut text = format_output(path, &sliced, display_start, raw, tag_opt.as_deref());
    if let Some(s) = suggestion {
        text.push_str(&s);
    }
    Ok((
        text,
        serde_json::json!({
            "path": path,
            "total_lines": total,
            "start_line": display_start + 1,
            "end_line": display_end,
            "raw": raw,
            "tag": tag_opt,
        }),
    ))
}

/// Windowed read for files larger than `READ_MAX_BYTES`. Reads only the
/// requested window plus context lines from the env, never the whole file.
/// Hashline anchoring and precise out-of-bounds hints are unavailable on this
/// path (the total line count is unknown without reading the whole file).
async fn read_file_windowed(
    path: &std::path::Path,
    start: usize,
    effective_limit: usize,
    raw: bool,
    offset: Option<usize>,
    limit: Option<usize>,
    ctx: &ToolContext,
) -> Result<(String, serde_json::Value), ToolError> {
    // Compute the display window: [start - leading_context, start + limit + trailing_context).
    let display_start = start.saturating_sub(RANGE_LEADING_CONTEXT_LINES);
    // Ask the env for at most (leading + limit + trailing) lines starting from
    // the top of the file. `read_text_lines` takes a max line count, not an
    // offset, so we over-read by `display_start` lines and slice. The cap
    // keeps the read bounded regardless of file size.
    let want = effective_limit + RANGE_LEADING_CONTEXT_LINES + RANGE_TRAILING_CONTEXT_LINES;
    let max_lines = display_start + want;
    let lines = match ctx
        .env
        .read_text_lines(path, Some(max_lines), ctx.abort.clone())
        .await
    {
        Ok(lines) => lines,
        Err(llm_harness_types::EnvError::InvalidUtf8(_)) => {
            let bytes = ctx
                .env
                .read_binary_file(path, ctx.abort.clone())
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;
            let n = bytes.len();
            return Ok((
                format!("[Binary file, {n} bytes]"),
                serde_json::json!({ "path": path, "bytes": n, "binary": true }),
            ));
        }
        Err(e) => return Err(ToolError::Execution(e.to_string())),
    };

    let available = lines.len();
    let display_end = (display_start + want).min(available);
    if display_start >= available {
        // Requested window is beyond what the env returned.
        return Ok((
            format!(
                "(out of bounds: offset {} is past the readable window of {available} lines; use :1 to start from the beginning)",
                offset.unwrap_or(1)
            ),
            serde_json::json!({
                "path": path,
                "total_lines": available,
                "start_line": display_start + 1,
                "end_line": display_end,
                "raw": raw,
                "tag": null,
                "large_file": true,
            }),
        ));
    }
    let sliced: Vec<&String> = lines[display_start..display_end].iter().collect();
    let text = format_output(path, &sliced, display_start, raw, None);
    Ok((
        text,
        serde_json::json!({
            "path": path,
            "total_lines": available,
            "start_line": display_start + 1,
            "end_line": display_end,
            "raw": raw,
            "tag": null,
            "large_file": true,
            "paginated": limit.is_some() || offset.is_some(),
        }),
    ))
}

/// Format the sliced lines into the tool output.
fn format_output(
    path: &std::path::Path,
    lines: &[&String],
    display_start: usize,
    raw: bool,
    tag: Option<&str>,
) -> String {
    if raw {
        // Plain text, lines joined with newlines, no header or numbering.
        let joined: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        return joined.join("\n");
    }

    let mut out = String::new();
    if let Some(tag) = tag {
        let display_path = path.to_string_lossy().replace('\\', "/");
        out.push_str(&format!("[{display_path}#{tag}]\n"));
    }
    for (i, line) in lines.iter().enumerate() {
        let line_no = display_start + i + 1;
        out.push_str(&format!("{line_no}:{line}"));
        if i + 1 < lines.len() {
            out.push('\n');
        }
    }
    out
}

/// List a directory and render its entries.
async fn read_directory(path: &std::path::Path, ctx: &ToolContext) -> Result<String, ToolError> {
    let mut entries = ctx
        .env
        .list_dir(path, ctx.abort.clone())
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;

    if entries.is_empty() {
        return Ok("(empty directory)".to_string());
    }

    // Sort by file name for stable output.
    entries.sort_by(|a, b| {
        a.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .cmp(b.path.file_name().and_then(|n| n.to_str()).unwrap_or(""))
    });

    let truncated = entries.len() > DIR_MAX_ENTRIES;
    if truncated {
        entries.truncate(DIR_MAX_ENTRIES);
    }

    let mut out = String::new();
    for (i, entry) in entries.iter().enumerate() {
        let name = entry
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if entry.is_dir {
            out.push_str(&format!("{name}/"));
        } else if entry.size > 0 {
            out.push_str(&format!("{name} ({} bytes)", entry.size));
        } else {
            out.push_str(&name);
        }
        if i + 1 < entries.len() {
            out.push('\n');
        }
    }
    if truncated {
        out.push_str("\n... (truncated)");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::MockExecutionEnv;
    use chrono::Utc;
    use llm_harness_types::{AssistantMessage, AssistantMessageKind};
    use std::path::PathBuf;
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
            tool_use_id: "tu_read".into(),
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

    /// Extract the single text block from a `ToolResult`.
    fn text_of(res: ToolResult) -> String {
        match res.model_content.as_slice() {
            [DataBlock::Text { text, .. }] => text.clone(),
            _ => panic!("expected a single text block"),
        }
    }

    #[tokio::test]
    async fn read_file_hashline_mode() {
        let env = Arc::new(MockExecutionEnv::new());
        let path = PathBuf::from("/tmp/mock/lines.txt");
        env.write_file_sync(&path, "alpha\nbeta\ngamma");

        let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
        let ctx = make_ctx(env.clone());
        let tool = ReadTool::new(Some(store.clone()));

        let res = tool
            .execute(serde_json::json!({ "path": "lines.txt" }), &ctx)
            .await
            .expect("read should succeed");
        let text = text_of(res);
        let header_line = text.lines().next().unwrap();
        assert!(
            header_line.starts_with("[/tmp/mock/lines.txt#"),
            "expected hashline header, got: {header_line}"
        );
        let tag = header_line
            .trim_start_matches('[')
            .rsplit('#')
            .next()
            .unwrap()
            .trim_end_matches(']');
        assert_eq!(tag.len(), 4, "tag should be 4 hex chars: {tag}");

        // Numbered rows follow.
        assert!(text.contains("1:alpha"), "missing line 1: {text}");
        assert!(text.contains("2:beta"), "missing line 2: {text}");
        assert!(text.contains("3:gamma"), "missing line 3: {text}");

        // Snapshot recorded with matching tag.
        let entry = store
            .read()
            .lookup(&path)
            .expect("snapshot should be recorded");
        assert_eq!(entry.tag, tag, "recorded tag should match header");
        assert_eq!(
            entry.lines,
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
        );
    }

    #[tokio::test]
    async fn read_file_raw_mode() {
        let env = Arc::new(MockExecutionEnv::new());
        let path = PathBuf::from("/tmp/mock/raw.txt");
        env.write_file_sync(&path, "one\ntwo\nthree");

        let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
        let ctx = make_ctx(env.clone());
        let tool = ReadTool::new(Some(store.clone()));

        let res = tool
            .execute(serde_json::json!({ "path": "raw.txt", "raw": true }), &ctx)
            .await
            .expect("read should succeed");
        let text = text_of(res);

        // No header, no line numbers — just plain text.
        assert_eq!(text, "one\ntwo\nthree");
        assert!(
            store.read().lookup(&path).is_none(),
            "raw mode must not record a snapshot"
        );
    }

    #[tokio::test]
    async fn read_file_no_store_numbered() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("/tmp/mock/nostore.txt"), "foo\nbar");

        let ctx = make_ctx(env.clone());
        let tool = ReadTool::new(None);

        let res = tool
            .execute(serde_json::json!({ "path": "nostore.txt" }), &ctx)
            .await
            .expect("read should succeed");
        let text = text_of(res);

        // No header, but lines are still numbered.
        assert!(!text.contains('#'), "no store means no header: {text}");
        assert!(text.contains("1:foo"), "missing line 1: {text}");
        assert!(text.contains("2:bar"), "missing line 2: {text}");
    }

    #[tokio::test]
    async fn read_file_with_offset_limit() {
        let env = Arc::new(MockExecutionEnv::new());
        // 10 lines: line1..line10
        let content = (1..=10)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        env.write_file_sync(&PathBuf::from("/tmp/mock/range.txt"), &content);

        let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
        let ctx = make_ctx(env.clone());
        let tool = ReadTool::new(Some(store.clone()));

        // offset=5, limit=2 → raw range [5,6) (1-indexed lines 5..6).
        // Expanded by 1 leading + 3 trailing → display lines 4..9.
        let res = tool
            .execute(
                serde_json::json!({ "path": "range.txt", "offset": 5, "limit": 2 }),
                &ctx,
            )
            .await
            .expect("read should succeed");
        let text = text_of(res);

        // Leading context line 4, requested lines 5-6, trailing context 7-9.
        assert!(text.contains("4:line4"), "missing leading context: {text}");
        assert!(text.contains("5:line5"), "missing requested line 5: {text}");
        assert!(text.contains("6:line6"), "missing requested line 6: {text}");
        assert!(text.contains("9:line9"), "missing trailing context: {text}");
        assert!(
            !text.contains("3:line3"),
            "should not include line 3: {text}"
        );
        assert!(
            !text.contains("10:line10"),
            "should not include line 10: {text}"
        );
    }

    #[tokio::test]
    async fn read_directory() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("/tmp/mock/dir/a.txt"), "aaa");
        env.write_file_sync(&PathBuf::from("/tmp/mock/dir/b.txt"), "bbbb");
        env.mkdir_sync(&PathBuf::from("/tmp/mock/dir"));

        let ctx = make_ctx(env.clone());
        let tool = ReadTool::new(None);

        let res = tool
            .execute(serde_json::json!({ "path": "dir" }), &ctx)
            .await
            .expect("read should succeed");
        let text = text_of(res);

        assert!(text.contains("a.txt (3 bytes)"), "missing a.txt: {text}");
        assert!(text.contains("b.txt (4 bytes)"), "missing b.txt: {text}");
    }

    #[tokio::test]
    async fn read_empty_directory() {
        let env = Arc::new(MockExecutionEnv::new());
        env.mkdir_sync(&PathBuf::from("/tmp/mock/empty"));

        let ctx = make_ctx(env.clone());
        let tool = ReadTool::new(None);

        let res = tool
            .execute(serde_json::json!({ "path": "empty" }), &ctx)
            .await
            .expect("read should succeed");
        let text = text_of(res);
        assert_eq!(text, "(empty directory)");
    }

    #[tokio::test]
    async fn read_nonexistent_file() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        let tool = ReadTool::new(None);

        let err = tool
            .execute(serde_json::json!({ "path": "nope.txt" }), &ctx)
            .await
            .expect_err("missing file should error");
        assert_eq!(err.code, "execution_failed");
    }

    #[tokio::test]
    async fn read_large_file_uses_windowed_path() {
        // File > READ_MAX_BYTES (4 MiB) must take the windowed path: no
        // snapshot tag, `large_file: true`, and only the requested window
        // surfaced. We build a 5-line file where each line is ~1 MiB so the
        // total exceeds the cap without holding many lines.
        let env = Arc::new(MockExecutionEnv::new());
        let path = PathBuf::from("/tmp/mock/big.txt");
        let big_line = "x".repeat(1_100_000);
        let content = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            big_line, big_line, big_line, big_line, big_line
        );
        env.write_file_sync(&path, &content);

        let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
        let ctx = make_ctx(env);
        let tool = ReadTool::new(Some(store.clone()));

        // Request lines 2-3 (offset=2, limit=2). The windowed path should
        // return those lines without reading the whole file into the snapshot.
        let res = tool
            .execute(
                serde_json::json!({ "path": "big.txt", "offset": 2, "limit": 2 }),
                &ctx,
            )
            .await
            .expect("windowed read should succeed");

        assert_eq!(res.details["large_file"], true);
        assert!(res.details["tag"].is_null(), "no tag on windowed path");
        // start_line should reflect offset 2 minus 1 leading context line = 1.
        assert_eq!(res.details["start_line"], 1);
    }
}
