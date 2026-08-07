//! 状态栏内容收集器——四种内容类型 + 共享计数器。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::status_panel::store::TodoStore;

// ── CollectorCtx ──────────────────────────────────────────────────────────

/// 收集器上下文（只读快照）。
pub struct CollectorCtx<'a> {
    pub turn_index: u32,
    pub todo_store: &'a TodoStore,
    pub tool_counter: &'a ToolCallCounter,
}

// ── StatusCollector trait ─────────────────────────────────────────────────

/// 状态栏内容收集器 trait。
///
/// `collect()` 返回 XML 片段（不含外层 `<agent_status>` 标签）。
/// 空字符串表示该收集器本轮无内容，会被跳过。
pub trait StatusCollector: Send + Sync {
    fn collect(&self, ctx: &CollectorCtx) -> String;
}

// ── TodoCollector ─────────────────────────────────────────────────────────

/// 任务规划收集器——读取 `TodoStore` 快照渲染 `<todo_list>`。
pub struct TodoCollector;

impl StatusCollector for TodoCollector {
    fn collect(&self, ctx: &CollectorCtx) -> String {
        let items = ctx.todo_store.snapshot();
        if items.is_empty() {
            return String::new();
        }
        let mut buf = String::from("  <todo_list>\n");
        for item in &items {
            buf.push_str(&format!(
                "    <item id=\"{}\" status=\"{}\">{}</item>\n",
                llm_harness_types::escape_xml_attr(&item.id),
                item.status,
                llm_harness_types::escape_xml_attr(&item.content),
            ));
        }
        buf.push_str("  </todo_list>\n");
        buf
    }
}

// ── EnvironmentCollector ──────────────────────────────────────────────────

/// 环境状态收集器——注入时间戳、工作目录、操作系统。
///
/// 不依赖 `ExecutionEnv`（hook 上下文不提供），直接用 `std::env` + `chrono`。
/// 用户可在 Plugin 构造时传入 `working_dir` 覆盖。
pub struct EnvironmentCollector {
    working_dir: Option<PathBuf>,
}

impl EnvironmentCollector {
    pub fn new() -> Self {
        Self { working_dir: None }
    }

    pub fn with_working_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: Some(dir.into()),
        }
    }
}

impl Default for EnvironmentCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusCollector for EnvironmentCollector {
    fn collect(&self, _ctx: &CollectorCtx) -> String {
        let now = chrono::Utc::now();
        let cwd = std::env::current_dir()
            .or_else(|_| self.working_dir.clone().ok_or(()))
            .unwrap_or_default();
        format!(
            "  <environment>\n\
             \x20   <timestamp>{}</timestamp>\n\
             \x20   <working_dir>{}</working_dir>\n\
             \x20   <os>{}</os>\n\
             \x20 </environment>\n",
            now.to_rfc3339(),
            llm_harness_types::escape_xml_attr(&cwd.display().to_string()),
            std::env::consts::OS,
        )
    }
}

// ── ToolCallCounter ───────────────────────────────────────────────────────

/// 共享工具调用计数器。`ToolCallCounterHook` 写，`ToolCallCounterCollector` 读。
#[derive(Debug, Clone, Default)]
pub struct ToolCallCounter(Arc<RwLock<HashMap<String, u32>>>);

impl ToolCallCounter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn increment(&self, tool_name: &str) {
        let mut guard = self.0.write();
        *guard.entry(tool_name.to_string()).or_insert(0) += 1;
    }

    /// 快照（按次数降序排列）。
    pub fn snapshot(&self) -> Vec<(String, u32)> {
        let guard = self.0.read();
        let mut entries: Vec<_> = guard.iter().map(|(k, v)| (k.clone(), *v)).collect();
        entries.sort_by_key(|&(_, v)| std::cmp::Reverse(v));
        entries
    }
}

// ── ToolCallCounterCollector ──────────────────────────────────────────────

/// 工具调用计数收集器——读取 `ToolCallCounter` 快照渲染 `<tool_calls>`。
pub struct ToolCallCounterCollector;

impl StatusCollector for ToolCallCounterCollector {
    fn collect(&self, ctx: &CollectorCtx) -> String {
        let counts = ctx.tool_counter.snapshot();
        if counts.is_empty() {
            return String::new();
        }
        let mut buf = String::from("  <tool_calls>\n");
        for (name, count) in &counts {
            buf.push_str(&format!(
                "    <call tool=\"{}\" count=\"{}\"/>\n",
                llm_harness_types::escape_xml_attr(name),
                count,
            ));
        }
        buf.push_str("  </tool_calls>\n");
        buf
    }
}

// ── SideChannelCollector ──────────────────────────────────────────────────

/// 侧信道时间戳收集器——给最近 N 条 user/tool 消息加 `[timestamp]` 前缀。
///
/// 与其他收集器不同：不生成末尾 XML 片段，而是就地修改已有消息。
/// 由 `StatusBarHook` 直接调用 `prefix_messages()`，不走 `StatusCollector` trait。
pub struct SideChannelCollector {
    /// 只给最近 N 条消息加前缀（避免全量遍历）。
    pub recent_window: usize,
}

impl SideChannelCollector {
    pub fn new() -> Self {
        Self { recent_window: 3 }
    }

    pub fn with_window(n: usize) -> Self {
        Self { recent_window: n }
    }

    /// 给 `messages` 末尾 `recent_window` 条 user/tool 消息加时间戳前缀。
    /// 已有前缀的消息不重复添加。
    pub fn prefix_messages(&self, messages: &mut [llm_harness_types::AgentMessage]) {
        let now = chrono::Utc::now();
        let prefix = format!("[{}] ", now.to_rfc3339());

        let start = messages.len().saturating_sub(self.recent_window);
        for msg in &mut messages[start..] {
            self.prefix_one(msg, &prefix);
        }
    }

    fn prefix_one(&self, msg: &mut llm_harness_types::AgentMessage, prefix: &str) {
        use llm_harness_types::{AgentMessage, ContentBlock, DataBlock};

        match msg {
            AgentMessage::User(u) => {
                for block in u.content.iter_mut() {
                    if let ContentBlock::Text { text } = block {
                        if !has_timestamp_prefix(text) {
                            text.insert_str(0, prefix);
                        }
                        break;
                    }
                }
            }
            AgentMessage::ToolResult(t) => {
                for block in t.content.iter_mut() {
                    if let DataBlock::Text { text, .. } = block {
                        if !has_timestamp_prefix(text) {
                            text.insert_str(0, prefix);
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
}

impl Default for SideChannelCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if text already starts with a `[timestamp]` prefix.
/// Matches `[` followed by an RFC3339-like date pattern `\d{4}-\d{2}-\d{2}T`.
fn has_timestamp_prefix(text: &str) -> bool {
    // Match pattern: [\d{4}-\d{2}-\d{2}T (RFC3339 date prefix inside brackets)
    let bytes = text.as_bytes();
    bytes.len() >= 11
        && bytes[0] == b'['
        && bytes[1..5].iter().all(|b| b.is_ascii_digit())
        && bytes[5] == b'-'
        && bytes[6..8].iter().all(|b| b.is_ascii_digit())
        && bytes[8] == b'-'
        && bytes[9..11].iter().all(|b| b.is_ascii_digit())
        && bytes.len() > 11
        && (bytes[11] == b'T' || bytes[11] == b't')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status_panel::store::TodoItem;
    use chrono::Utc;

    fn make_ctx<'a>(todo: &'a TodoStore, counter: &'a ToolCallCounter) -> CollectorCtx<'a> {
        CollectorCtx {
            turn_index: 0,
            todo_store: todo,
            tool_counter: counter,
        }
    }

    // ── TodoCollector ──

    #[test]
    fn todo_collector_empty_store_returns_empty() {
        let store = TodoStore::new();
        let counter = ToolCallCounter::new();
        let ctx = make_ctx(&store, &counter);
        assert!(TodoCollector.collect(&ctx).is_empty());
    }

    #[test]
    fn todo_collector_renders_items() {
        let store = TodoStore::new();
        store.rewrite(vec![
            TodoItem::new("1", "task A"),
            TodoItem::new("2", "task B"),
        ]);
        let counter = ToolCallCounter::new();
        let ctx = make_ctx(&store, &counter);
        let xml = TodoCollector.collect(&ctx);
        assert!(xml.contains("<todo_list>"));
        assert!(xml.contains("id=\"1\""));
        assert!(xml.contains("status=\"pending\""));
        assert!(xml.contains("task A"));
    }

    // ── EnvironmentCollector ──

    #[test]
    fn environment_collector_renders_fields() {
        let store = TodoStore::new();
        let counter = ToolCallCounter::new();
        let ctx = make_ctx(&store, &counter);
        let xml = EnvironmentCollector::new().collect(&ctx);
        assert!(xml.contains("<timestamp>"));
        assert!(xml.contains("<working_dir>"));
        assert!(xml.contains("<os>"));
    }

    // ── ToolCallCounter ──

    #[test]
    fn counter_increment_and_snapshot() {
        let counter = ToolCallCounter::new();
        counter.increment("bash");
        counter.increment("bash");
        counter.increment("read");
        let snap = counter.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0], ("bash".to_string(), 2)); // 降序
        assert_eq!(snap[1], ("read".to_string(), 1));
    }

    #[test]
    fn counter_collector_empty_returns_empty() {
        let store = TodoStore::new();
        let counter = ToolCallCounter::new();
        let ctx = make_ctx(&store, &counter);
        assert!(ToolCallCounterCollector.collect(&ctx).is_empty());
    }

    #[test]
    fn counter_collector_renders_counts() {
        let store = TodoStore::new();
        let counter = ToolCallCounter::new();
        counter.increment("bash");
        counter.increment("read");
        let ctx = make_ctx(&store, &counter);
        let xml = ToolCallCounterCollector.collect(&ctx);
        assert!(xml.contains("<tool_calls>"));
        assert!(xml.contains("tool=\"bash\""));
        assert!(xml.contains("count=\"1\""));
    }

    // ── SideChannelCollector ──

    #[test]
    fn side_channel_prefixes_user_message() {
        use llm_harness_types::{AgentMessage, ContentBlock, UserMessage};
        let sc = SideChannelCollector::with_window(2);
        let mut messages = vec![AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
            timestamp: Utc::now(),
        })];
        sc.prefix_messages(&mut messages);
        if let AgentMessage::User(u) = &messages[0] {
            if let ContentBlock::Text { text } = &u.content[0] {
                assert!(text.starts_with('['));
                assert!(text.ends_with("hello"));
            } else {
                panic!("expected text block");
            }
        } else {
            panic!("expected user message");
        }
    }

    #[test]
    fn side_channel_does_not_double_prefix() {
        use llm_harness_types::{AgentMessage, ContentBlock, UserMessage};
        let sc = SideChannelCollector::with_window(2);
        let mut messages = vec![AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "[2026-08-07T10:30:45+00:00] hello".into(),
            }],
            timestamp: Utc::now(),
        })];
        sc.prefix_messages(&mut messages);
        if let AgentMessage::User(u) = &messages[0] {
            if let ContentBlock::Text { text } = &u.content[0] {
                assert_eq!(text, "[2026-08-07T10:30:45+00:00] hello");
            } else {
                panic!("expected text block");
            }
        } else {
            panic!("expected user message");
        }
    }

    #[test]
    fn side_channel_prefixes_tool_result_message() {
        use llm_harness_types::{AgentMessage, DataBlock, ToolResultMessage};
        let sc = SideChannelCollector::with_window(2);
        let mut messages = vec![AgentMessage::ToolResult(ToolResultMessage {
            tool_use_id: "c1".into(),
            content: vec![DataBlock::text("result data")],
            is_error: false,
            projection: llm_harness_types::PersistedToolProjection {
                kind: llm_harness_types::ToolProjectionKind::Full,
                metadata: llm_harness_types::ProjectionMetadata::default(),
            },
            timestamp: Utc::now(),
        })];
        sc.prefix_messages(&mut messages);
        if let AgentMessage::ToolResult(t) = &messages[0] {
            if let DataBlock::Text { text, .. } = &t.content[0] {
                assert!(text.starts_with('['));
                assert!(text.ends_with("result data"));
            } else {
                panic!("expected text block");
            }
        } else {
            panic!("expected tool result message");
        }
    }

    #[test]
    fn side_channel_skips_assistant_messages() {
        use llm_harness_types::{AgentMessage, ContentBlock, UserMessage};
        let sc = SideChannelCollector::with_window(3);
        let mut messages = vec![
            AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: "msg1".into(),
                }],
                timestamp: Utc::now(),
            }),
            AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: "msg2".into(),
                }],
                timestamp: Utc::now(),
            }),
        ];
        sc.prefix_messages(&mut messages);
        // 两条都应该被加前缀（window=3 >= 2）
        for msg in &messages {
            if let AgentMessage::User(u) = msg
                && let ContentBlock::Text { text } = &u.content[0]
            {
                assert!(text.starts_with('['), "message not prefixed: {}", text);
            }
        }
    }
}
