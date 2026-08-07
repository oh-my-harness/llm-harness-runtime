//! hook.rs — StatusBarHook (impl TransformContextHook).
//!
//! 每轮 LLM 调用前：
//! 1. （可选）侧信道给最近 N 条消息加时间戳前缀
//! 2. 按 `UpdateStrategy` 决定是否移除上一轮的状态栏消息
//! 3. 跑所有 `StatusCollector`，拼成 `<agent_status>` XML
//! 4. 作为 `AgentMessage::User` 追加到上下文末尾

use futures::future::BoxFuture;
use llm_harness_types::{
    AgentContext, AgentError, AgentMessage, ContentBlock, TransformContextCtx,
    TransformContextHook, UserMessage,
};

use crate::status_panel::collectors::{
    CollectorCtx, SideChannelCollector, StatusCollector, ToolCallCounter,
};
use crate::status_panel::format::{is_status_text, render_status_xml};
use crate::status_panel::store::TodoStore;

/// 状态栏更新策略。
#[derive(Debug, Clone)]
pub enum UpdateStrategy {
    /// 每轮移除旧状态栏再追加新的。
    Replace,
    /// 纯追加，旧状态栏留在轨迹中。
    Append,
    /// 自动选择（默认）。
    /// trajectory_len > threshold || status_size < min_bytes → Replace
    /// 否则 Append。
    Auto {
        trajectory_threshold: usize,
        status_min_bytes: usize,
    },
}

impl Default for UpdateStrategy {
    fn default() -> Self {
        UpdateStrategy::Auto {
            trajectory_threshold: 20,
            status_min_bytes: 2048,
        }
    }
}

pub struct StatusBarHook {
    collectors: Vec<Box<dyn StatusCollector>>,
    side_channel: Option<SideChannelCollector>,
    strategy: UpdateStrategy,
    todo_store: TodoStore,
    tool_counter: ToolCallCounter,
}

impl StatusBarHook {
    pub fn new(
        collectors: Vec<Box<dyn StatusCollector>>,
        side_channel: Option<SideChannelCollector>,
        strategy: UpdateStrategy,
        todo_store: TodoStore,
        tool_counter: ToolCallCounter,
    ) -> Self {
        Self {
            collectors,
            side_channel,
            strategy,
            todo_store,
            tool_counter,
        }
    }

    /// 判断消息是否为状态栏消息。
    fn is_status_message(msg: &AgentMessage) -> bool {
        if let AgentMessage::User(u) = msg {
            u.content.iter().any(|c| {
                if let ContentBlock::Text { text } = c {
                    is_status_text(text)
                } else {
                    false
                }
            })
        } else {
            false
        }
    }

    /// 从末尾移除最近一条状态栏消息（如果有）。
    fn remove_last_status(messages: &mut Vec<AgentMessage>) {
        if let Some(pos) = messages.iter().rposition(Self::is_status_message) {
            messages.remove(pos);
        }
    }

    /// 决定是否使用 Replace 策略。
    fn resolve_replace(&self, ctx: &AgentContext) -> bool {
        match &self.strategy {
            UpdateStrategy::Replace => true,
            UpdateStrategy::Append => false,
            UpdateStrategy::Auto {
                trajectory_threshold,
                status_min_bytes,
            } => {
                let len = ctx.messages.len();
                let status_size = self.estimate_status_size();
                len > *trajectory_threshold || status_size < *status_min_bytes
            }
        }
    }

    fn estimate_status_size(&self) -> usize {
        // 粗略估计：每个 collector 输出约 200-500 bytes
        self.collectors.len() * 400
    }
}

impl TransformContextHook for StatusBarHook {
    fn transform<'a>(
        &'a self,
        ctx: TransformContextCtx<'a>,
    ) -> BoxFuture<'a, Result<AgentContext, AgentError>> {
        let mut context = ctx.context;

        // 1. 侧信道：给最近 N 条消息加时间戳前缀
        if let Some(sc) = &self.side_channel {
            sc.prefix_messages(&mut context.messages);
        }

        // 2. 更新策略：决定是否移除旧状态栏
        let replace = self.resolve_replace(&context);
        if replace {
            Self::remove_last_status(&mut context.messages);
        }

        // 3. 收集器：生成 XML 片段
        let collector_ctx = CollectorCtx {
            turn_index: ctx.turn_index,
            todo_store: &self.todo_store,
            tool_counter: &self.tool_counter,
        };
        let mut fragments = Vec::with_capacity(self.collectors.len());
        for collector in &self.collectors {
            let fragment = collector.collect(&collector_ctx);
            if !fragment.is_empty() {
                fragments.push(fragment);
            }
        }

        // 4. 渲染并追加
        if !fragments.is_empty() {
            let xml = render_status_xml(&fragments);
            context.messages.push(AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text { text: xml }],
                timestamp: chrono::Utc::now(),
            }));
        }

        Box::pin(async move { Ok(context) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status_panel::collectors::{
        EnvironmentCollector, TodoCollector, ToolCallCounterCollector,
    };
    use crate::status_panel::store::{TodoItem, TodoStore};
    use llm_harness_types::{RunContext, RunRequest};
    use tokio_util::sync::CancellationToken;

    fn make_hook(strategy: UpdateStrategy, todo_store: TodoStore) -> StatusBarHook {
        let counter = ToolCallCounter::new();
        StatusBarHook::new(
            vec![
                Box::new(TodoCollector),
                Box::new(EnvironmentCollector::new()),
                Box::new(ToolCallCounterCollector),
            ],
            None,
            strategy,
            todo_store,
            counter,
        )
    }

    fn make_transform_ctx(context: AgentContext) -> TransformContextCtx<'static> {
        let run = Box::leak(Box::new(RunContext::new(RunRequest::default())));
        let abort = CancellationToken::new();
        TransformContextCtx {
            run,
            turn_index: 0,
            abort,
            context,
        }
    }

    #[tokio::test]
    async fn appends_status_bar_to_empty_context() {
        let store = TodoStore::new();
        store.rewrite(vec![TodoItem::new("1", "task A")]);
        let hook = make_hook(UpdateStrategy::Append, store);

        let ctx = make_transform_ctx(AgentContext {
            system_prompt: None,
            messages: vec![],
        });
        let result = hook.transform(ctx).await.unwrap();

        let last = result.messages.last().unwrap();
        assert!(StatusBarHook::is_status_message(last));
    }

    #[tokio::test]
    async fn replace_removes_old_status_before_appending() {
        let store = TodoStore::new();
        store.rewrite(vec![TodoItem::new("1", "task A")]);
        let hook = make_hook(UpdateStrategy::Replace, store);

        let old_status = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "<agent_status>\n  <old/>\n</agent_status>".into(),
            }],
            timestamp: chrono::Utc::now(),
        });

        let ctx = make_transform_ctx(AgentContext {
            system_prompt: None,
            messages: vec![old_status],
        });
        let result = hook.transform(ctx).await.unwrap();

        let status_count = result
            .messages
            .iter()
            .filter(|m| StatusBarHook::is_status_message(m))
            .count();
        assert_eq!(status_count, 1);

        let last = result.messages.last().unwrap();
        if let AgentMessage::User(u) = last {
            if let ContentBlock::Text { text } = &u.content[0] {
                assert!(text.contains("task A"));
                assert!(!text.contains("<old/>"));
            }
        }
    }

    #[tokio::test]
    async fn append_keeps_old_status() {
        let store = TodoStore::new();
        store.rewrite(vec![TodoItem::new("1", "task A")]);
        let hook = make_hook(UpdateStrategy::Append, store);

        let old_status = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "<agent_status>\n  <old/>\n</agent_status>".into(),
            }],
            timestamp: chrono::Utc::now(),
        });

        let ctx = make_transform_ctx(AgentContext {
            system_prompt: None,
            messages: vec![old_status],
        });
        let result = hook.transform(ctx).await.unwrap();

        let status_count = result
            .messages
            .iter()
            .filter(|m| StatusBarHook::is_status_message(m))
            .count();
        assert_eq!(status_count, 2);
    }

    #[tokio::test]
    async fn no_fragments_no_injection() {
        let hook = StatusBarHook::new(
            vec![],
            None,
            UpdateStrategy::Append,
            TodoStore::new(),
            ToolCallCounter::new(),
        );

        let ctx = make_transform_ctx(AgentContext {
            system_prompt: None,
            messages: vec![],
        });
        let result = hook.transform(ctx).await.unwrap();
        assert!(result.messages.is_empty());
    }

    #[tokio::test]
    async fn auto_strategy_uses_replace_for_long_trajectory() {
        let store = TodoStore::new();
        store.rewrite(vec![TodoItem::new("1", "task A")]);
        let counter = ToolCallCounter::new();
        let hook = StatusBarHook::new(
            vec![Box::new(TodoCollector)],
            None,
            UpdateStrategy::Auto {
                trajectory_threshold: 5,
                status_min_bytes: 0,
            },
            store.clone(),
            counter,
        );

        let mut messages: Vec<AgentMessage> = (0..9)
            .map(|_| {
                AgentMessage::User(UserMessage {
                    content: vec![ContentBlock::Text { text: "msg".into() }],
                    timestamp: chrono::Utc::now(),
                })
            })
            .collect();
        messages.push(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "<agent_status><old/></agent_status>".into(),
            }],
            timestamp: chrono::Utc::now(),
        }));

        let ctx = make_transform_ctx(AgentContext {
            system_prompt: None,
            messages,
        });
        let result = hook.transform(ctx).await.unwrap();

        let status_count = result
            .messages
            .iter()
            .filter(|m| StatusBarHook::is_status_message(m))
            .count();
        assert_eq!(status_count, 1);
    }
}
