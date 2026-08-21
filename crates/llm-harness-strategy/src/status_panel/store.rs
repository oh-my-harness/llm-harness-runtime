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

impl TodoStatus {
    /// Whether this status represents an "open" item (still needs work).
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Pending | Self::InProgress)
    }
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

    /// 追加单个 item。
    pub fn add(&self, item: TodoItem) {
        self.0.write().push(item);
    }

    /// 指定 id 的 item 是否已存在。
    pub fn contains_id(&self, id: &str) -> bool {
        self.0.read().iter().any(|i| i.id == id)
    }

    /// 清空全部 item（对应 `clear` action）。
    pub fn clear(&self) {
        self.0.write().clear();
    }

    /// 是否有未完成项（pending 或 in_progress）。
    pub fn has_open_items(&self) -> bool {
        self.0.read().iter().any(|i| i.status.is_open())
    }

    /// 当前 item 数量。
    pub fn len(&self) -> usize {
        self.0.read().len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.0.read().is_empty()
    }

    /// 格式化 todolist 为文本快照，供 BeforeRunHook 注入。
    ///
    /// 所有 open item 完整显示，closed item 只显示最近若干条。
    pub fn format_snapshot(&self) -> String {
        let items = self.0.read();
        if items.is_empty() {
            return "Your todolist is empty.".to_string();
        }

        let mut open: Vec<&TodoItem> = Vec::new();
        let mut closed: Vec<&TodoItem> = Vec::new();
        for item in items.iter() {
            if item.status.is_open() {
                open.push(item);
            } else {
                closed.push(item);
            }
        }

        let mut out = String::from("## Current Todolist\n\n");
        for item in &open {
            out.push_str(&format!(
                "- [{}] {} — {}\n",
                item.status, item.id, item.content
            ));
        }

        let max_closed = 10;
        let total_closed = closed.len();
        let shown = total_closed.min(max_closed);
        if shown > 0 {
            let start = total_closed - shown;
            for item in closed[start..].iter() {
                out.push_str(&format!(
                    "- [{}] {} — {}\n",
                    item.status, item.id, item.content
                ));
            }
        }

        let hidden = total_closed.saturating_sub(shown);
        if hidden > 0 {
            out.push_str(&format!(
                "\n({} older completed/cancelled items not shown)\n",
                hidden
            ));
        }

        out
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

#[cfg(test)]
mod new_method_tests {
    use super::*;

    #[test]
    fn is_open_pending_and_in_progress() {
        assert!(TodoStatus::Pending.is_open());
        assert!(TodoStatus::InProgress.is_open());
        assert!(!TodoStatus::Completed.is_open());
        assert!(!TodoStatus::Cancelled.is_open());
    }

    #[test]
    fn add_appends_item() {
        let store = TodoStore::new();
        store.add(TodoItem::new("1", "A"));
        store.add(TodoItem::new("2", "B"));
        let snap = store.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[1].id, "2");
    }

    #[test]
    fn contains_id_returns_true_for_existing() {
        let store = TodoStore::new();
        store.add(TodoItem::new("1", "A"));
        assert!(store.contains_id("1"));
        assert!(!store.contains_id("2"));
    }

    #[test]
    fn clear_removes_all_items() {
        let store = TodoStore::new();
        store.add(TodoItem::new("1", "A"));
        store.add(TodoItem::new("2", "B"));
        assert_eq!(store.len(), 2);
        store.clear();
        assert!(store.is_empty());
    }

    #[test]
    fn clear_on_empty_store_is_noop() {
        let store = TodoStore::new();
        store.clear();
        assert!(store.is_empty());
    }

    #[test]
    fn has_open_items_true_when_pending() {
        let store = TodoStore::new();
        store.add(TodoItem::new("1", "A"));
        assert!(store.has_open_items());
    }

    #[test]
    fn has_open_items_false_when_all_closed() {
        let store = TodoStore::new();
        store.add(TodoItem {
            id: "1".into(),
            content: "A".into(),
            status: TodoStatus::Completed,
            updated_at: chrono::Utc::now(),
        });
        assert!(!store.has_open_items());
    }

    #[test]
    fn has_open_items_false_when_empty() {
        let store = TodoStore::new();
        assert!(!store.has_open_items());
    }

    #[test]
    fn len_and_is_empty() {
        let store = TodoStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        store.add(TodoItem::new("1", "A"));
        assert!(!store.is_empty());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn format_snapshot_empty() {
        let store = TodoStore::new();
        assert_eq!(store.format_snapshot(), "Your todolist is empty.");
    }

    #[test]
    fn format_snapshot_shows_open_items() {
        let store = TodoStore::new();
        store.add(TodoItem::new("1", "open task"));
        store.add(TodoItem {
            id: "2".into(),
            content: "done task".into(),
            status: TodoStatus::Completed,
            updated_at: chrono::Utc::now(),
        });
        let snap = store.format_snapshot();
        assert!(snap.contains("## Current Todolist"));
        assert!(snap.contains("1"));
        assert!(snap.contains("open task"));
        assert!(snap.contains("2"));
        assert!(snap.contains("done task"));
    }

    #[test]
    fn format_snapshot_hides_old_closed_items() {
        let store = TodoStore::new();
        // 12 closed items — only last 10 shown.
        for i in 0..12 {
            store.add(TodoItem {
                id: format!("c{}", i),
                content: format!("closed {}", i),
                status: TodoStatus::Completed,
                updated_at: chrono::Utc::now(),
            });
        }
        let snap = store.format_snapshot();
        assert!(snap.contains("2 older completed/cancelled items not shown"));
    }
}
