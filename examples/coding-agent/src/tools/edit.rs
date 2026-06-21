use futures::future::BoxFuture;
use llm_harness_types::*;
use serde_json::Value;

/// Replace an exact string in a file. The old_string must appear exactly once.
pub struct EditTool {
    schema: Value,
}

impl EditTool {
    pub fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to edit (relative or absolute)"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Exact text to replace. Must appear exactly once in the file."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text."
                    }
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        }
    }
}

impl Default for EditTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Perform an exact string replacement in a file. \
         old_string must appear exactly once in the file."
    }

    fn parameters_schema(&self) -> &Value {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move { edit_file(args, ctx).await })
    }
}

async fn edit_file(args: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
    let path_str = args["file_path"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("file_path required".into()))?;
    let old_string = args["old_string"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("old_string required".into()))?;
    let new_string = args["new_string"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("new_string required".into()))?;

    let abs_path = resolve_path(path_str, ctx.env.working_dir());

    let content = ctx
        .env
        .read_text_file(&abs_path, ctx.abort.clone())
        .await
        .map_err(|e| ToolError::Execution(format!("Could not read {}: {}", path_str, e)))?;

    let count = content.matches(old_string).count();
    match count {
        0 => {
            return Err(ToolError::Execution(format!(
                "old_string not found in {}",
                path_str
            )));
        }
        1 => {}
        n => {
            return Err(ToolError::Execution(format!(
                "old_string appears {} times in {}; must appear exactly once",
                n, path_str
            )));
        }
    }

    let new_content = content.replacen(old_string, new_string, 1);

    ctx.env
        .write_file(&abs_path, new_content.as_bytes(), ctx.abort.clone())
        .await
        .map_err(|e| ToolError::Execution(format!("Could not write {}: {}", path_str, e)))?;

    Ok(ToolResult {
        content: vec![ContentBlock::Text {
            text: format!("Successfully edited {}", path_str),
        }],
        details: Value::Null,
        terminate: false,
    })
}

fn resolve_path(path: &str, cwd: &std::path::Path) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_runtime_sandbox_os::OsEnv;
    use std::io::Write as IoWrite;
    use std::sync::Arc;
    use tempfile::NamedTempFile;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn make_ctx(env: Arc<dyn ExecutionEnv>) -> ToolContext {
        let (tx, _rx) = mpsc::channel(4);
        ToolContext {
            env,
            abort: CancellationToken::new(),
            tool_use_id: "test-id".into(),
            turn_index: 0,
            assistant_message: Arc::new(AssistantMessage {
                content: vec![],
                stop_reason: None,
                timestamp: chrono::Utc::now(),
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
    async fn edit_replaces_unique_string() {
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "hello world").unwrap();
        let path = tmp.path().to_path_buf();

        let env = Arc::new(OsEnv::new(path.parent().unwrap().to_path_buf()));
        let ctx = make_ctx(env);

        let tool = EditTool::new();
        let args = serde_json::json!({
            "file_path": path.to_str().unwrap(),
            "old_string": "world",
            "new_string": "Rust"
        });
        tool.execute(args, &ctx).await.unwrap();

        let updated = std::fs::read_to_string(&path).unwrap();
        assert_eq!(updated, "hello Rust");
    }

    #[tokio::test]
    async fn edit_fails_when_string_not_found() {
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "hello world").unwrap();
        let path = tmp.path().to_path_buf();

        let env = Arc::new(OsEnv::new(path.parent().unwrap().to_path_buf()));
        let ctx = make_ctx(env);

        let tool = EditTool::new();
        let args = serde_json::json!({
            "file_path": path.to_str().unwrap(),
            "old_string": "nonexistent",
            "new_string": "x"
        });
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {:?}", err);
    }

    #[tokio::test]
    async fn edit_fails_when_string_not_unique() {
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "abc abc abc").unwrap();
        let path = tmp.path().to_path_buf();

        let env = Arc::new(OsEnv::new(path.parent().unwrap().to_path_buf()));
        let ctx = make_ctx(env);

        let tool = EditTool::new();
        let args = serde_json::json!({
            "file_path": path.to_str().unwrap(),
            "old_string": "abc",
            "new_string": "xyz"
        });
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("3 times"), "got: {:?}", err);
    }
}
