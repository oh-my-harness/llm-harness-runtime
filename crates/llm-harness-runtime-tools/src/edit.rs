//! The `edit` tool: apply line-level swap/insert/delete operations against a
//! file, using hashline tags from the [`FileSnapshotStore`] to detect stale
//! edits.
//!
//! An `EditOperation` is one of `swap`, `insert`, or `delete`. Operations are
//! applied in sequence, with line numbers (1-indexed) resolved against the
//! state after each prior operation. When a `tag` and a snapshot store are
//! supplied, the stored tag must match — a mismatch means the caller is
//! operating against stale content and is rejected as a stale snapshot.

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolError, ToolFailure, ToolResult};
use parking_lot::RwLock;
use serde::Deserialize;

use crate::resolve_path;
use crate::snapshot::FileSnapshotStore;

/// A single line-level edit operation applied against a file.
///
/// Line numbers are 1-indexed and refer to the state after any prior
/// operations in the same `edit` call. `serde` tags the enum on the `type`
/// field using snake_case (`swap`, `insert`, `delete`).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EditOperation {
    /// Replace lines `start..=end` (1-indexed, inclusive) with `lines`.
    Swap {
        /// First line to replace (1-indexed, inclusive).
        start: usize,
        /// Last line to replace (1-indexed, inclusive). Must be >= `start`.
        end: usize,
        /// Replacement lines.
        lines: Vec<String>,
    },
    /// Insert `lines` starting before line `start` (1-indexed). `start == len + 1`
    /// appends at the end.
    Insert {
        /// Position (1-indexed) at which to insert; existing line `start` (if any)
        /// is shifted down.
        start: usize,
        /// Lines to insert.
        lines: Vec<String>,
    },
    /// Delete lines `start..=end` (1-indexed, inclusive).
    Delete {
        /// First line to delete (1-indexed, inclusive).
        start: usize,
        /// Last line to delete (1-indexed, inclusive). Must be >= `start`.
        end: usize,
    },
}

/// `edit` tool — apply line-level swap/insert/delete operations to a file.
///
/// When a [`FileSnapshotStore`] is attached and a `tag` is provided, the tool
/// verifies the tag matches the recorded snapshot for the path; a mismatch
/// (stale content) is rejected. On success the snapshot is refreshed with the
/// new lines so the returned tag can anchor a subsequent edit.
///
/// **Output normalization:** the file is always rewritten as the joined lines
/// plus a single trailing `\n`, following the POSIX text-file convention. This
/// means an edit to a file that originally lacked a trailing newline will
/// introduce one; callers that care about byte-exact preservation should use
/// `write` instead.
pub struct EditTool {
    store: Option<Arc<RwLock<FileSnapshotStore>>>,
}

impl EditTool {
    /// Create an `EditTool`. Pass `Some(store)` to enable hashline tag
    /// verification and refresh; pass `None` for standalone use (always reads
    /// the current file content from the env).
    pub fn new(store: Option<Arc<RwLock<FileSnapshotStore>>>) -> Self {
        Self { store }
    }
}

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Apply line-level swap, insert, or delete operations to a file with optional staleness checking.\n\n\
         Use when: modifying specific lines in an existing file; making targeted edits without rewriting the whole file; applying multiple edits in sequence.\n\
         Don't use for: creating new files (use write); replacing entire file content (use write); byte-exact binary edits.\n\
         Returns: The resolved file path and number of operations applied. Fails if a staleness tag is provided and the file has changed since the last read.\n\n\
         Examples:\n\
         {\"path\": \"src/main.rs\", \"operations\": [{\"type\": \"swap\", \"start\": 5, \"end\": 8, \"lines\": [\"    println!(\\\"hello\\\");\"]}]}\n\
         {\"path\": \"src/main.rs\", \"operations\": [{\"type\": \"insert\", \"start\": 10, \"lines\": [\"// TODO: refactor\", \"\"]}, {\"type\": \"delete\", \"start\": 15, \"end\": 18}]}\n\
         {\"path\": \"src/main.rs\", \"tag\": \"A1B2\", \"operations\": [{\"type\": \"delete\", \"start\": 20, \"end\": 25}]}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file (relative or absolute). e.g. \"src/main.rs\""
                    },
                    "tag": {
                        "type": "string",
                        "description": "Optional 4-hex staleness tag from a prior read (prevents editing a stale file). e.g. \"A1B2\""
                    },
                    "operations": {
                        "type": "array",
                        "description": "Ordered list of swap/insert/delete operations. Line numbers are 1-indexed and refer to the state after prior operations. e.g. [{\"type\":\"swap\",\"start\":5,\"end\":8,\"lines\":[\"new line\"]}]",
                        "items": {
                            "type": "object",
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "const": "swap" },
                                        "start": { "type": "integer", "minimum": 1 },
                                        "end": { "type": "integer", "minimum": 1 },
                                        "lines": { "type": "array", "items": { "type": "string" } }
                                    },
                                    "required": ["type", "start", "end", "lines"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "const": "insert" },
                                        "start": { "type": "integer", "minimum": 1 },
                                        "lines": { "type": "array", "items": { "type": "string" } }
                                    },
                                    "required": ["type", "start", "lines"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "const": "delete" },
                                        "start": { "type": "integer", "minimum": 1 },
                                        "end": { "type": "integer", "minimum": 1 }
                                    },
                                    "required": ["type", "start", "end"]
                                }
                            ]
                        }
                    }
                },
                "required": ["path", "operations"]
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
            let tag = args
                .get("tag")
                .and_then(serde_json::Value::as_str)
                .map(|s| s.to_string());
            let ops_value = args.get("operations").ok_or_else(|| {
                ToolError::InvalidArguments("missing array field 'operations'".into())
            })?;
            let operations: Vec<EditOperation> = serde_json::from_value(ops_value.clone())
                .map_err(|e| ToolError::InvalidArguments(format!("invalid operations: {e}")))?;

            let working_dir = ctx.env.working_dir();
            let path = resolve_path(working_dir, path_str);

            // Acquire the base lines. When a tag and store are present, verify
            // the tag against the recorded snapshot; if the snapshot was
            // invalidated (e.g. by a write since the read), fall back to the
            // env. Otherwise read directly from the env.
            let mut lines: Vec<String> = if let Some(tag) = &tag
                && let Some(store) = &self.store
            {
                let entry_opt = store.read().lookup(&path);
                match entry_opt {
                    Some(entry) => {
                        if &entry.tag != tag {
                            return Err(ToolError::Execution(format!(
                                "stale snapshot: tag {} does not match recorded tag {} for {}",
                                tag,
                                entry.tag,
                                path.display()
                            ))
                            .into());
                        }
                        entry.lines
                    }
                    None => {
                        // No snapshot entry (invalidated): fall back to env.
                        ctx.env
                            .read_text_lines(&path, None, ctx.abort.clone())
                            .await
                            .map_err(|e| ToolError::Execution(e.to_string()))?
                    }
                }
            } else {
                ctx.env
                    .read_text_lines(&path, None, ctx.abort.clone())
                    .await
                    .map_err(|e| ToolError::Execution(e.to_string()))?
            };

            let original_len = lines.len();
            for op in &operations {
                apply_operation(&mut lines, op)?;
            }
            let final_len = lines.len();

            // Write the result: lines joined with "\n" plus a trailing newline.
            let mut content = lines.join("\n");
            content.push('\n');
            ctx.env
                .write_file(&path, content.as_bytes(), ctx.abort.clone())
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;

            // Refresh the snapshot so the caller can chain edits by tag.
            if let Some(store) = &self.store {
                store.read().record(&path, lines.clone());
            }

            let n_ops = operations.len();
            let text = format!(
                "Edited {}: {} operation(s) applied, {} lines → {} lines",
                path.display(),
                n_ops,
                original_len,
                final_len
            );
            Ok(ToolResult::full(
                vec![DataBlock::text(text)],
                serde_json::json!({
                    "path": path,
                    "operations": n_ops,
                    "original_lines": original_len,
                    "final_lines": final_len,
                }),
                false,
            ))
        })
    }
}

/// Apply a single `EditOperation` to `lines` in place, validating bounds.
///
/// Line numbers are 1-indexed and refer to the current state of `lines`
/// (i.e. after any prior operations in the same call).
fn apply_operation(lines: &mut Vec<String>, op: &EditOperation) -> Result<(), ToolError> {
    let len = lines.len();
    match op {
        EditOperation::Swap {
            start,
            end,
            lines: new_lines,
        } => {
            if *start < 1 || *end < *start || *end > len {
                return Err(ToolError::Execution(format!(
                    "swap out of bounds: start={start}, end={end}, len={len}"
                )));
            }
            lines.splice(start - 1..*end, new_lines.clone());
        }
        EditOperation::Insert {
            start,
            lines: new_lines,
        } => {
            if *start < 1 || *start > len + 1 {
                return Err(ToolError::Execution(format!(
                    "insert out of bounds: start={start}, len={len}"
                )));
            }
            for (i, line) in new_lines.iter().enumerate() {
                lines.insert(start - 1 + i, line.clone());
            }
        }
        EditOperation::Delete { start, end } => {
            if *start < 1 || *end < *start || *end > len {
                return Err(ToolError::Execution(format!(
                    "delete out of bounds: start={start}, end={end}, len={len}"
                )));
            }
            lines.drain(start - 1..*end);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::MockExecutionEnv;
    use chrono::Utc;
    use llm_harness_types::{AssistantMessage, AssistantMessageKind, ExecutionEnv};
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
            tool_use_id: "tu_edit".into(),
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

    /// Read a mock file's current text contents.
    async fn read_file(env: &MockExecutionEnv, path: &str) -> String {
        env.read_text_file(&PathBuf::from(path), CancellationToken::new())
            .await
            .expect("file should exist")
    }

    #[tokio::test]
    async fn edit_swap_single() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("/tmp/mock/f.txt"), "alpha\nbeta\ngamma");
        let ctx = make_ctx(env.clone());
        let tool = EditTool::new(None);

        let res = tool
            .execute(
                serde_json::json!({
                    "path": "f.txt",
                    "operations": [
                        { "type": "swap", "start": 2, "end": 2, "lines": ["BETA"] }
                    ]
                }),
                &ctx,
            )
            .await
            .expect("edit should succeed");
        let text = text_of(res);
        assert!(text.contains("1 operation(s) applied"), "got: {text}");
        assert_eq!(
            read_file(&env, "/tmp/mock/f.txt").await,
            "alpha\nBETA\ngamma\n"
        );
    }

    #[tokio::test]
    async fn edit_insert() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("/tmp/mock/f.txt"), "alpha\nbeta\ngamma");
        let ctx = make_ctx(env.clone());
        let tool = EditTool::new(None);

        tool.execute(
            serde_json::json!({
                "path": "f.txt",
                "operations": [
                    { "type": "insert", "start": 2, "lines": ["X", "Y"] }
                ]
            }),
            &ctx,
        )
        .await
        .expect("edit should succeed");

        assert_eq!(
            read_file(&env, "/tmp/mock/f.txt").await,
            "alpha\nX\nY\nbeta\ngamma\n"
        );
    }

    #[tokio::test]
    async fn edit_delete() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("/tmp/mock/f.txt"), "alpha\nbeta\ngamma");
        let ctx = make_ctx(env.clone());
        let tool = EditTool::new(None);

        tool.execute(
            serde_json::json!({
                "path": "f.txt",
                "operations": [
                    { "type": "delete", "start": 2, "end": 2 }
                ]
            }),
            &ctx,
        )
        .await
        .expect("edit should succeed");

        assert_eq!(read_file(&env, "/tmp/mock/f.txt").await, "alpha\ngamma\n");
    }

    #[tokio::test]
    async fn edit_multiple_operations() {
        // 4-line file: a, b, c, d
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("/tmp/mock/f.txt"), "a\nb\nc\nd");
        let ctx = make_ctx(env.clone());
        let tool = EditTool::new(None);

        // delete(1,1) → b,c,d ; insert(1,Z) → Z,b,c,d ; swap(3,3,D) → Z,b,D,d
        tool.execute(
            serde_json::json!({
                "path": "f.txt",
                "operations": [
                    { "type": "delete", "start": 1, "end": 1 },
                    { "type": "insert", "start": 1, "lines": ["Z"] },
                    { "type": "swap", "start": 3, "end": 3, "lines": ["D"] }
                ]
            }),
            &ctx,
        )
        .await
        .expect("edit should succeed");

        assert_eq!(read_file(&env, "/tmp/mock/f.txt").await, "Z\nb\nD\nd\n");
    }

    #[tokio::test]
    async fn edit_tag_match_succeeds() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("/tmp/mock/f.txt"), "alpha\nbeta\ngamma");
        let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
        let path = PathBuf::from("/tmp/mock/f.txt");
        // Record a snapshot (as ReadTool would).
        let tag = store
            .read()
            .record(&path, vec!["alpha".into(), "beta".into(), "gamma".into()]);

        let ctx = make_ctx(env.clone());
        let tool = EditTool::new(Some(store.clone()));

        let res = tool
            .execute(
                serde_json::json!({
                    "path": "f.txt",
                    "tag": tag,
                    "operations": [
                        { "type": "swap", "start": 2, "end": 2, "lines": ["BETA"] }
                    ]
                }),
                &ctx,
            )
            .await
            .expect("edit should succeed with matching tag");
        let text = text_of(res);
        assert!(text.contains("1 operation(s) applied"), "got: {text}");
        assert_eq!(
            read_file(&env, "/tmp/mock/f.txt").await,
            "alpha\nBETA\ngamma\n"
        );

        // Snapshot refreshed with the new lines.
        let entry = store
            .read()
            .lookup(&path)
            .expect("snapshot should be refreshed");
        assert_eq!(
            entry.lines,
            vec!["alpha".to_string(), "BETA".to_string(), "gamma".to_string()]
        );
    }

    #[tokio::test]
    async fn edit_tag_mismatch_errors() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("/tmp/mock/f.txt"), "alpha\nbeta\ngamma");
        let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
        let path = PathBuf::from("/tmp/mock/f.txt");
        store
            .read()
            .record(&path, vec!["alpha".into(), "beta".into(), "gamma".into()]);

        let ctx = make_ctx(env.clone());
        let tool = EditTool::new(Some(store.clone()));

        let err = tool
            .execute(
                serde_json::json!({
                    "path": "f.txt",
                    "tag": "dead",
                    "operations": [
                        { "type": "swap", "start": 1, "end": 1, "lines": ["Z"] }
                    ]
                }),
                &ctx,
            )
            .await
            .expect_err("stale tag should error");
        assert_eq!(err.code, "execution_failed");
        // File untouched.
        assert_eq!(
            read_file(&env, "/tmp/mock/f.txt").await,
            "alpha\nbeta\ngamma"
        );
    }

    #[tokio::test]
    async fn edit_no_store_no_tag_works() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("/tmp/mock/f.txt"), "alpha\nbeta\ngamma");
        let ctx = make_ctx(env.clone());
        let tool = EditTool::new(None);

        tool.execute(
            serde_json::json!({
                "path": "f.txt",
                "operations": [
                    { "type": "insert", "start": 1, "lines": ["TOP"] }
                ]
            }),
            &ctx,
        )
        .await
        .expect("edit should succeed");

        assert_eq!(
            read_file(&env, "/tmp/mock/f.txt").await,
            "TOP\nalpha\nbeta\ngamma\n"
        );
    }

    #[tokio::test]
    async fn edit_out_of_bounds_errors() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("/tmp/mock/f.txt"), "alpha\nbeta\ngamma");
        let ctx = make_ctx(env.clone());
        let tool = EditTool::new(None);

        // Delete range beyond file length (len=3, end=5).
        let err = tool
            .execute(
                serde_json::json!({
                    "path": "f.txt",
                    "operations": [
                        { "type": "delete", "start": 2, "end": 5 }
                    ]
                }),
                &ctx,
            )
            .await
            .expect_err("out-of-bounds delete should error");
        assert_eq!(err.code, "execution_failed");
        // File untouched.
        assert_eq!(
            read_file(&env, "/tmp/mock/f.txt").await,
            "alpha\nbeta\ngamma"
        );
    }

    #[tokio::test]
    async fn edit_after_write_falls_back_to_env() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("/tmp/mock/f.txt"), "alpha\nbeta\ngamma");
        let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
        let path = PathBuf::from("/tmp/mock/f.txt");
        let old_tag = store
            .read()
            .record(&path, vec!["alpha".into(), "beta".into(), "gamma".into()]);

        // Simulate a WriteTool invalidating the snapshot and changing the file.
        store.read().invalidate(&path);
        env.write_file_sync(&PathBuf::from("/tmp/mock/f.txt"), "alpha\nCHANGED\ngamma");

        let ctx = make_ctx(env.clone());
        let tool = EditTool::new(Some(store.clone()));

        // Edit with the old tag — snapshot is gone, so the tool falls back to
        // the env and edits the current (post-write) content.
        let res = tool
            .execute(
                serde_json::json!({
                    "path": "f.txt",
                    "tag": old_tag,
                    "operations": [
                        { "type": "swap", "start": 2, "end": 2, "lines": ["NEW"] }
                    ]
                }),
                &ctx,
            )
            .await
            .expect("edit should fall back to env and succeed");
        let text = text_of(res);
        assert!(text.contains("1 operation(s) applied"), "got: {text}");
        assert_eq!(
            read_file(&env, "/tmp/mock/f.txt").await,
            "alpha\nNEW\ngamma\n"
        );
    }
}
