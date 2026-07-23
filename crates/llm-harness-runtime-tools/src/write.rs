//! The `write` tool: overwrite a file with new content, creating parent
//! directories as needed and invalidating any stale [`FileSnapshotStore`]
//! entry so a subsequent `edit` cannot silently apply against the old tag.

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolError, ToolFailure, ToolResult};
use parking_lot::RwLock;

use crate::resolve_path;
use crate::snapshot::FileSnapshotStore;

/// `write` tool — overwrite a file with new content.
///
/// Resolves the target path against the env working dir, creates any missing
/// parent directories, writes the bytes, and — when a snapshot store is
/// attached — invalidates the recorded snapshot for the path so a stale
/// `edit` against the pre-write content is rejected.
pub struct WriteTool {
    store: Option<Arc<RwLock<FileSnapshotStore>>>,
}

impl WriteTool {
    /// Create a `WriteTool`. Pass `Some(store)` to wire up snapshot
    /// invalidation; pass `None` for standalone use.
    pub fn new(store: Option<Arc<RwLock<FileSnapshotStore>>>) -> Self {
        Self { store }
    }
}

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Overwrite a file with the given content, creating parent directories as needed."
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        S.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write (relative to the working dir or absolute)."
                    },
                    "content": {
                        "type": "string",
                        "description": "The full new content to write to the file."
                    }
                },
                "required": ["path", "content"]
            })
        })
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
            let content = args
                .get("content")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ToolError::InvalidArguments("missing string field 'content'".into())
                })?;

            let working_dir = ctx.env.working_dir();
            let path = resolve_path(working_dir, path_str);

            // Create parent directories when the file is not landing directly
            // in the working dir. `create_dir` is recursive; the working dir
            // itself already exists.
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
                && parent != working_dir
            {
                ctx.env
                    .create_dir(parent, true, ctx.abort.clone())
                    .await
                    .map_err(|e| ToolError::Execution(e.to_string()))?;
            }

            let bytes = content.as_bytes();
            let n = bytes.len();
            ctx.env
                .write_file(&path, bytes, ctx.abort.clone())
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;

            // Invalidate any snapshot recorded for this path so a subsequent
            // edit can't match against the pre-write content tag.
            if let Some(store) = &self.store {
                store.read().invalidate(&path);
            }

            let text = format!("Wrote {n} bytes to {}", path.display());
            Ok(ToolResult::full(
                vec![DataBlock::text(text)],
                serde_json::json!({
                    "path": path,
                    "bytes": n,
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
            tool_use_id: "tu_write".into(),
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
    async fn write_new_file() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env.clone());
        let tool = WriteTool::new(None);

        let res = tool
            .execute(
                serde_json::json!({
                    "path": "hello.txt",
                    "content": "hi there"
                }),
                &ctx,
            )
            .await
            .expect("write should succeed");

        let text = match res.model_content.as_slice() {
            [DataBlock::Text { text, .. }] => text.clone(),
            _ => panic!("expected a single text block"),
        };
        assert!(text.contains("Wrote 8 bytes to"), "got: {text}");

        let written = env
            .read_text_file(&PathBuf::from("/tmp/mock/hello.txt"), ctx.abort.clone())
            .await
            .expect("file should exist");
        assert_eq!(written, "hi there");
    }

    #[tokio::test]
    async fn write_overwrites_existing() {
        let env = Arc::new(MockExecutionEnv::new());
        env.write_file_sync(&PathBuf::from("/tmp/mock/greet.txt"), "old content");
        let ctx = make_ctx(env.clone());
        let tool = WriteTool::new(None);

        tool.execute(
            serde_json::json!({
                "path": "greet.txt",
                "content": "new content"
            }),
            &ctx,
        )
        .await
        .expect("write should succeed");

        let written = env
            .read_text_file(&PathBuf::from("/tmp/mock/greet.txt"), ctx.abort.clone())
            .await
            .expect("file should exist");
        assert_eq!(written, "new content");
    }

    #[tokio::test]
    async fn write_invalidates_snapshot() {
        let env = Arc::new(MockExecutionEnv::new());
        let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
        let path = PathBuf::from("/tmp/mock/snap.txt");

        // Seed a snapshot for the path, mimicking what ReadTool would record.
        store
            .write()
            .record(&path, vec!["old line 1".into(), "old line 2".into()]);
        assert!(
            store.read().lookup(&path).is_some(),
            "snapshot should exist"
        );

        let ctx = make_ctx(env.clone());
        let tool = WriteTool::new(Some(store.clone()));

        tool.execute(
            serde_json::json!({
                "path": "snap.txt",
                "content": "fresh content"
            }),
            &ctx,
        )
        .await
        .expect("write should succeed");

        assert!(
            store.read().lookup(&path).is_none(),
            "snapshot must be invalidated after write"
        );
    }

    #[tokio::test]
    async fn write_missing_path_arg() {
        let env = Arc::new(MockExecutionEnv::new());
        let ctx = make_ctx(env);
        let tool = WriteTool::new(None);

        let err = tool
            .execute(
                serde_json::json!({
                    "content": "no path here"
                }),
                &ctx,
            )
            .await
            .expect_err("missing path should error");

        assert_eq!(err.code, "invalid_arguments");
        assert!(err.model_message.contains("path"));
    }
}
