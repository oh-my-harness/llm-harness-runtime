#![allow(clippy::result_large_err)]
//! todo_tool.rs — TodoWriteTool (impl Tool).
//!
//! 只读写 TodoStore（内存），不碰文件系统。`ToolLevel::User`。

use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolLevel, ToolResult};

use crate::status_panel::store::{TodoItem, TodoStatus, TodoStore};
static TODO_WRITE_SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["rewrite", "update"] },
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "content": { "type": "string" },
                        "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"] }
                    },
                    "required": ["id", "content", "status"]
                }
            },
            "id": { "type": "string" },
            "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"] }
        },
        "required": ["action"]
    })
});

pub struct TodoWriteTool {
    store: TodoStore,
}

impl TodoWriteTool {
    pub fn new(store: TodoStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &TodoStore {
        &self.store
    }
}

impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::User
    }

    fn description(&self) -> &str {
        "Create or update the agent's TODO list. Use `rewrite` to replace the \
         entire list, or `update` to change a single item's status. \
         The current TODO list is shown in the <agent_status> block at the \
         end of the context each turn."
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &TODO_WRITE_SCHEMA
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        let result = run_todo_write(&self.store, &args);
        Box::pin(async move { result })
    }
}

fn run_todo_write(store: &TodoStore, args: &serde_json::Value) -> Result<ToolResult, ToolFailure> {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolFailure::invalid_arguments("missing `action` field"))?;

    match action {
        "rewrite" => {
            let items = parse_items(args)?;
            store.rewrite(items);
            Ok(ToolResult::full(
                vec![DataBlock::text("TODO list rewritten.")],
                serde_json::json!({ "action": "rewrite", "count": store.snapshot().len() }),
                false,
            ))
        }
        "update" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolFailure::invalid_arguments("missing `id` field"))?;
            let status = parse_status(args)?;
            if store.update_status(id, status) {
                Ok(ToolResult::full(
                    vec![DataBlock::text(format!(
                        "TODO `{}` updated to {}.",
                        id, status
                    ))],
                    serde_json::json!({ "action": "update", "id": id, "status": status }),
                    false,
                ))
            } else {
                Err(ToolFailure::new(
                    "not_found",
                    format!("TODO item `{}` not found", id),
                ))
            }
        }
        other => Err(ToolFailure::invalid_arguments(format!(
            "unknown action: `{}`",
            other
        ))),
    }
}

fn parse_items(args: &serde_json::Value) -> Result<Vec<TodoItem>, ToolFailure> {
    let arr = args
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ToolFailure::invalid_arguments("missing `items` array"))?;

    let now = chrono::Utc::now();
    arr.iter()
        .map(|item| {
            let id = item
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolFailure::invalid_arguments("item missing `id`"))?
                .to_string();
            let content = item
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolFailure::invalid_arguments("item missing `content`"))?
                .to_string();
            let status = parse_status(item)?;
            Ok(TodoItem {
                id,
                content,
                status,
                updated_at: now,
            })
        })
        .collect()
}

fn parse_status(v: &serde_json::Value) -> Result<TodoStatus, ToolFailure> {
    match v.get("status").and_then(|s| s.as_str()) {
        Some("pending") => Ok(TodoStatus::Pending),
        Some("in_progress") => Ok(TodoStatus::InProgress),
        Some("completed") => Ok(TodoStatus::Completed),
        Some("cancelled") => Ok(TodoStatus::Cancelled),
        Some(other) => Err(ToolFailure::invalid_arguments(format!(
            "unknown status: `{}`",
            other
        ))),
        None => Err(ToolFailure::invalid_arguments("missing `status` field")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_loop::test_utils::NoOpEnv;
    use llm_harness_types::{RunContext, RunRequest, ToolLevel};
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn make_ctx() -> ToolContext {
        ToolContext {
            run: std::sync::Arc::new(RunContext::new(RunRequest::default())),
            env: std::sync::Arc::new(NoOpEnv),
            abort: CancellationToken::new(),
            tool_use_id: "test".to_string(),
            turn_index: 0,
            assistant_message: std::sync::Arc::new(llm_harness_types::AssistantMessage {
                kind: llm_harness_types::AssistantMessageKind::Progress,
                message_id: "m1".into(),
                turn_id: "t1".into(),
                content: vec![],
                stop_reason: None,
                timestamp: chrono::Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: None,
                error_message: None,
            }),
            update_tx: mpsc::channel::<llm_harness_types::ToolProgress>(1).0,
        }
    }

    #[tokio::test]
    async fn rewrite_action_sets_items() {
        let store = TodoStore::new();
        let tool = TodoWriteTool::new(store.clone());
        let args = json!({
            "action": "rewrite",
            "items": [
                {"id": "1", "content": "task A", "status": "pending"},
                {"id": "2", "content": "task B", "status": "in_progress"}
            ]
        });
        let result = tool.execute(args, &make_ctx()).await.unwrap();
        assert!(!result.terminate);
        assert_eq!(store.snapshot().len(), 2);
    }

    #[tokio::test]
    async fn update_action_changes_status() {
        let store = TodoStore::new();
        store.rewrite(vec![TodoItem::new("1", "task A")]);
        let tool = TodoWriteTool::new(store.clone());
        let args = json!({
            "action": "update",
            "id": "1",
            "status": "completed"
        });
        let result = tool.execute(args, &make_ctx()).await.unwrap();
        assert!(!result.terminate);
        assert_eq!(store.snapshot()[0].status, TodoStatus::Completed);
    }

    #[tokio::test]
    async fn update_nonexistent_id_returns_failure() {
        let store = TodoStore::new();
        store.rewrite(vec![TodoItem::new("1", "task A")]);
        let tool = TodoWriteTool::new(store);
        let args = json!({
            "action": "update",
            "id": "nonexistent",
            "status": "completed"
        });
        let result = tool.execute(args, &make_ctx()).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, "not_found");
    }

    #[tokio::test]
    async fn missing_action_returns_failure() {
        let store = TodoStore::new();
        let tool = TodoWriteTool::new(store);
        let args = json!({});
        let result = tool.execute(args, &make_ctx()).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, "invalid_arguments");
    }

    #[tokio::test]
    async fn invalid_status_returns_failure() {
        let store = TodoStore::new();
        store.rewrite(vec![TodoItem::new("1", "task A")]);
        let tool = TodoWriteTool::new(store);
        let args = json!({
            "action": "update",
            "id": "1",
            "status": "invalid_status"
        });
        let result = tool.execute(args, &make_ctx()).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, "invalid_arguments");
    }

    #[test]
    fn tool_name_is_todo_write() {
        let tool = TodoWriteTool::new(TodoStore::new());
        assert_eq!(tool.name(), "todo_write");
    }

    #[test]
    fn tool_level_is_user() {
        let tool = TodoWriteTool::new(TodoStore::new());
        assert_eq!(tool.level(), ToolLevel::User);
    }

    #[test]
    fn tool_schema_has_action_field() {
        let tool = TodoWriteTool::new(TodoStore::new());
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").unwrap().get("action").is_some());
    }
}
