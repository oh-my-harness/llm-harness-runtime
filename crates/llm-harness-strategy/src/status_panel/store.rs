//! TODO 共享存储；TodoWriteTool 写，TodoCollector 读。

use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl std::fmt::Display for TodoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::InProgress => write!(f, "in_progress"),
            Self::Completed => write!(f, "completed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
    pub updated_at: DateTime<Utc>,
}

impl TodoItem {
    /// 创建新 TODO 项，状态默认 Pending，时间戳为当前。
    pub fn new(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            status: TodoStatus::Pending,
            updated_at: Utc::now(),
        }
    }
}

/// 共享 TODO 存储。`TodoWriteTool` 写，`TodoCollector` 读。
#[derive(Debug, Clone)]
pub struct TodoStore(Arc<RwLock<Vec<TodoItem>>>);

impl TodoStore {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(Vec::new())))
    }

    /// 整体替换（对应 `rewrite_todo_list` action）。
    pub fn rewrite(&self, items: Vec<TodoItem>) {
        let mut guard = self.0.write();
        *guard = items;
    }

    /// 更新单项状态（对应 `update_todo_status` action）。
    /// 返回 `false` 表示 id 未找到。
    pub fn update_status(&self, id: &str, status: TodoStatus) -> bool {
        let mut guard = self.0.write();
        for item in guard.iter_mut() {
            if item.id == id {
                item.status = status;
                item.updated_at = Utc::now();
                return true;
            }
        }
        false
    }

    /// 快照（收集器读取）。
    pub fn snapshot(&self) -> Vec<TodoItem> {
        self.0.read().clone()
    }
}

impl Default for TodoStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_replaces_all_items() {
        let store = TodoStore::new();
        store.rewrite(vec![
            TodoItem::new("1", "task A"),
            TodoItem::new("2", "task B"),
        ]);
        let snap = store.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].id, "1");
        assert_eq!(snap[0].status, TodoStatus::Pending);
    }

    #[test]
    fn update_status_changes_existing_item() {
        let store = TodoStore::new();
        store.rewrite(vec![TodoItem::new("1", "task A")]);
        assert!(store.update_status("1", TodoStatus::Completed));
        let snap = store.snapshot();
        assert_eq!(snap[0].status, TodoStatus::Completed);
    }

    #[test]
    fn update_status_returns_false_for_missing_id() {
        let store = TodoStore::new();
        store.rewrite(vec![TodoItem::new("1", "task A")]);
        assert!(!store.update_status("nonexistent", TodoStatus::Completed));
    }

    #[test]
    fn rewrite_clears_previous_items() {
        let store = TodoStore::new();
        store.rewrite(vec![TodoItem::new("1", "task A")]);
        store.rewrite(vec![]);
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn store_clones_share_state() {
        let store = TodoStore::new();
        let clone = store.clone();
        store.rewrite(vec![TodoItem::new("1", "task A")]);
        assert_eq!(clone.snapshot().len(), 1);
    }
}
