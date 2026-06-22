use futures::future::BoxFuture;
use llm_harness_types::*;
use serde_json::Value;

use super::resolve_path;

/// Write (create or overwrite) a file with given content.
pub struct WriteTool {
    schema: Value,
}

impl WriteTool {
    pub fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to write (relative or absolute)"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["file_path", "content"]
            }),
        }
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating it or overwriting it if it exists. \
         Parent directories are created automatically."
    }

    fn parameters_schema(&self) -> &Value {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move { write_file(args, ctx).await })
    }
}

async fn write_file(args: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
    let path_str = args["file_path"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("file_path required".into()))?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("content required".into()))?;

    let abs_path = resolve_path(path_str, ctx.env.working_dir());

    if let Some(parent) = abs_path.parent() {
        ctx.env
            .create_dir(parent, true, ctx.abort.clone())
            .await
            .map_err(|e| ToolError::Execution(format!("Could not create directories: {}", e)))?;
    }

    ctx.env
        .write_file(&abs_path, content.as_bytes(), ctx.abort.clone())
        .await
        .map_err(|e| ToolError::Execution(format!("Could not write {}: {}", path_str, e)))?;

    Ok(ToolResult {
        content: vec![ContentBlock::Text {
            text: format!("Successfully wrote {} bytes to {}", content.len(), path_str),
        }],
        details: Value::Null,
        terminate: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_helpers;
    use llm_harness_runtime_sandbox_os::OsEnv;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn write_creates_new_file() {
        let dir = TempDir::new().unwrap();
        let env = Arc::new(OsEnv::new(dir.path().to_path_buf()));
        let ctx = test_helpers::make_ctx(env);

        let target = dir.path().join("new_file.txt");
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "file_path": target.to_str().unwrap(),
            "content": "hello from write"
        });
        tool.execute(args, &ctx).await.unwrap();

        let text = std::fs::read_to_string(&target).unwrap();
        assert_eq!(text, "hello from write");
    }

    #[tokio::test]
    async fn write_overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("existing.txt");
        std::fs::write(&target, "old content").unwrap();

        let env = Arc::new(OsEnv::new(dir.path().to_path_buf()));
        let ctx = test_helpers::make_ctx(env);

        let tool = WriteTool::new();
        let args = serde_json::json!({
            "file_path": target.to_str().unwrap(),
            "content": "new content"
        });
        tool.execute(args, &ctx).await.unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new content");
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("a/b/c/file.txt");

        let env = Arc::new(OsEnv::new(dir.path().to_path_buf()));
        let ctx = test_helpers::make_ctx(env);

        let tool = WriteTool::new();
        let args = serde_json::json!({
            "file_path": target.to_str().unwrap(),
            "content": "nested"
        });
        tool.execute(args, &ctx).await.unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "nested");
    }
}
