// crates/llm-harness-runtime/src/event.rs
//! 通用事件唤醒机制。
//!
//! runtime 只提供 `EventStream` trait（极简，只有 `next()`）和
//! `WaitForExternalEventTool`。恢复语义归业务层。
//!
//! task 调 `WaitForExternalEventTool` 后，tool 在 `execute` 里
//! `stream.next().await` 阻塞。event 到达 → tool 返回 → loop 续跑，
//! 无需手动 `resume()`。

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use llm_harness_types::{ContentBlock, Tool, ToolContext, ToolError, ToolExecutionMode, ToolResult};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::task::TaskId;
use crate::task_store::TaskStore;

/// 通用事件流抽象。业务层实现。
/// timer / IO 完成 / 上游 task 完成 / 外部信号等来源均可。
/// runtime 只定义运行时阻塞语义，不定义恢复语义。
///
/// `Send` 但非 `Sync`——持有可变的等待状态（如 `mpsc::Receiver`），单线程消费。
pub trait EventStream: Send {
    /// 阻塞等待下一个事件。返回 None 表示流结束（不再有事件）。
    /// 用 BoxFuture 而非 `async fn`，保证 trait 是 object-safe
    /// （`Box<dyn EventStream>` 可用），与代码库 `ShouldStopHook`/`Tool` 签名风格一致。
    fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<Event>>;
}

/// 事件载荷。content 给 LLM 看，details 供程序判断。
#[derive(Debug, Clone)]
pub struct Event {
    /// 与 ToolResult.content 同构。
    pub content: Vec<ContentBlock>,
    /// 结构化数据，供程序判断。
    pub details: Value,
}

/// 通用事件等待 tool。泛化 `WaitForPeerMessageTool` 的通用入口。
///
/// task 调用此 tool 后，tool 在 `execute` 里 `stream.next().await` 阻塞。
/// event 到达 → tool 返回 `ToolResult` → agent loop 自然进入下一个 turn 续跑。
/// 无需手动 `resume()`。
///
/// 崩溃恢复（业务层职责）：runtime 不自动化恢复，但提供落盘通道
/// （`TaskStore.save_event_wait`/`load_event_wait`）。tool 启动等待时写入
/// descriptor，重启后业务层读出重建 EventStream。
pub struct WaitForExternalEventTool {
    /// 事件流来源。由业务层构造并注入。
    stream: Arc<Mutex<Box<dyn EventStream>>>,
    timeout: Duration,
    /// 业务层提供的等待描述符（可序列化）。
    /// tool 启动等待时写入 TaskStore，重启后业务层读出重建 EventStream。
    wait_descriptor: Value,
    task_store: Option<Arc<dyn TaskStore>>,
    task_id: TaskId,
}

impl WaitForExternalEventTool {
    /// 创建 tool。
    ///
    /// - `stream`：事件流来源（业务层构造）。
    /// - `wait_descriptor`：等待描述符（可序列化），用于崩溃恢复。
    /// - `task_store`：若为 `Some`，等待期间落盘 descriptor。
    /// - `task_id`：task 标识，用于 TaskStore 关联。
    pub fn new(
        stream: Arc<Mutex<Box<dyn EventStream>>>,
        wait_descriptor: Value,
        task_store: Option<Arc<dyn TaskStore>>,
        task_id: TaskId,
    ) -> Self {
        Self {
            stream,
            timeout: Duration::from_secs(300),
            wait_descriptor,
            task_store,
            task_id,
        }
    }

    /// 设置等待超时。超时返回提示文本（非错误），LLM 自决重试/放弃。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

fn wait_for_event_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

impl Tool for WaitForExternalEventTool {
    fn name(&self) -> &str {
        "wait_for_external_event"
    }

    fn description(&self) -> &str {
        "阻塞等待一个外部事件（如 CI 完成、IO 完成、上游 task 完成）。\
         事件到达后作为结果返回。超时返回提示文本。"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        S.get_or_init(wait_for_event_schema)
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            // 1. 落盘等待描述符（若注入了 task_store）。
            if let Some(store) = &self.task_store {
                if let Err(e) = store.save_event_wait(&self.task_id, &self.wait_descriptor).await {
                    // 落盘失败不阻断 tool 本身的等待语义（最坏情况是重启后无法恢复该次等待）。
                    eprintln!("[warn] failed to save event_wait descriptor: {e}");
                }
            }

            // 2. select! 等待：abort / event / timeout
            let mut stream = self.stream.lock().await;
            let result = tokio::select! {
                biased;
                _ = ctx.abort.cancelled() => {
                    Err(ToolError::Aborted)
                }
                ev = stream.next() => {
                    match ev {
                        Some(event) => Ok(ToolResult {
                            content: event.content,
                            details: event.details,
                            terminate: false,
                        }),
                        None => Err(ToolError::Execution("event stream closed".into())),
                    }
                }
                _ = tokio::time::sleep(self.timeout) => {
                    Ok(ToolResult {
                        content: vec![ContentBlock::Text {
                            text: "(no event within timeout)".into(),
                        }],
                        details: serde_json::json!({"status": "timeout"}),
                        terminate: false,
                    })
                }
            };

            // 3. 清理等待状态（正常返回或 abort 后）。
            //    落盘失败不阻塞（同上）。
            if let Some(store) = &self.task_store {
                if let Err(e) = store.clear_event_wait(&self.task_id).await {
                    eprintln!("[warn] failed to clear event_wait descriptor: {e}");
                }
            }

            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::ContentBlock;
    use crate::task::TaskId;
    use crate::task_store::JsonlTaskStore;
    use llm_harness_types::{Tool, ToolContext, ToolError};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    /// 用 mpsc channel 模拟事件流。
    struct ChannelStream {
        rx: tokio::sync::mpsc::Receiver<Event>,
    }

    impl EventStream for ChannelStream {
        fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<Event>> {
            Box::pin(async move { self.rx.recv().await })
        }
    }

    #[tokio::test]
    async fn channel_stream_yields_events() {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let mut stream = ChannelStream { rx };
        tx.send(Event {
            content: vec![ContentBlock::Text { text: "first".into() }],
            details: serde_json::json!({"n": 1}),
        }).await.unwrap();
        tx.send(Event {
            content: vec![ContentBlock::Text { text: "second".into() }],
            details: serde_json::json!({"n": 2}),
        }).await.unwrap();
        drop(tx);

        let e1 = stream.next().await.unwrap();
        assert_eq!(e1.details["n"], 1);
        let e2 = stream.next().await.unwrap();
        assert_eq!(e2.details["n"], 2);
        assert!(stream.next().await.is_none()); // 流结束
    }

    #[tokio::test]
    async fn boxed_stream_next_returns_none_when_closed() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Event>(1);
        drop(tx);
        let mut s: Box<dyn EventStream> = Box::new(ChannelStream { rx });
        assert!(s.next().await.is_none());
    }

    /// 构造一个最小的 ToolContext（无真实 env，仅 abort token）。
    fn make_tool_ctx(abort: CancellationToken) -> ToolContext {
        ToolContext {
            env: Arc::new(llm_harness_loop::test_utils::NoOpEnv),
            abort,
            tool_use_id: "tu_1".into(),
            turn_index: 0,
            assistant_message: Arc::new(empty_assistant_message()),
            update_tx: tokio::sync::mpsc::channel(1).0,
        }
    }

    fn empty_assistant_message() -> llm_harness_types::AssistantMessage {
        use chrono::Utc;
        llm_harness_types::AssistantMessage {
            content: vec![],
            stop_reason: None,
            timestamp: Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        }
    }

    #[tokio::test]
    async fn tool_returns_event_when_stream_yields() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Event>(1);
        let stream: Arc<Mutex<Box<dyn EventStream>>> =
            Arc::new(Mutex::new(Box::new(ChannelStream { rx })));
        let tool = WaitForExternalEventTool::new(
            stream,
            serde_json::json!({"source": "test"}),
            None,
            TaskId("t1".into()),
        )
        .with_timeout(Duration::from_secs(5));

        // 发送一个事件
        tx.send(Event {
            content: vec![ContentBlock::Text { text: "done".into() }],
            details: serde_json::json!({"status": "ok"}),
        }).await.unwrap();

        let abort = CancellationToken::new();
        let ctx = make_tool_ctx(abort);
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!result.terminate);
        // content 含 "done"
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "done"),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn tool_returns_timeout_text_when_no_event() {
        let (_tx, rx) = tokio::sync::mpsc::channel::<Event>(1);
        let stream: Arc<Mutex<Box<dyn EventStream>>> =
            Arc::new(Mutex::new(Box::new(ChannelStream { rx })));
        let tool = WaitForExternalEventTool::new(
            stream,
            serde_json::json!({}),
            None,
            TaskId("t1".into()),
        )
        .with_timeout(Duration::from_millis(50));

        let abort = CancellationToken::new();
        let ctx = make_tool_ctx(abort);
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        // 超时返回提示文本（非错误）
        match &result.content[0] {
            ContentBlock::Text { text } => assert!(text.contains("timeout") || text.contains("no ")),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn tool_returns_aborted_on_cancel() {
        let (_tx, rx) = tokio::sync::mpsc::channel::<Event>(1);
        let stream: Arc<Mutex<Box<dyn EventStream>>> =
            Arc::new(Mutex::new(Box::new(ChannelStream { rx })));
        let tool = WaitForExternalEventTool::new(
            stream,
            serde_json::json!({}),
            None,
            TaskId("t1".into()),
        )
        .with_timeout(Duration::from_secs(60));

        let abort = CancellationToken::new();
        let ctx = make_tool_ctx(abort.clone());
        // 立刻 abort
        abort.cancel();
        let result = tool.execute(serde_json::json!({}), &ctx).await;
        assert!(matches!(result, Err(ToolError::Aborted)));
    }

    #[tokio::test]
    async fn tool_persists_event_wait_descriptor() {
        let dir = TempDir::new().unwrap();
        let store: Arc<dyn TaskStore> =
            Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
        let (tx, rx) = tokio::sync::mpsc::channel::<Event>(1);
        let stream: Arc<Mutex<Box<dyn EventStream>>> =
            Arc::new(Mutex::new(Box::new(ChannelStream { rx })));
        let descriptor = serde_json::json!({"source": "ci", "job_id": "#12345"});
        let task_id = TaskId("t1".into());
        let tool = WaitForExternalEventTool::new(
            stream,
            descriptor.clone(),
            Some(store.clone()),
            task_id.clone(),
        )
        .with_timeout(Duration::from_secs(5));

        tx.send(Event {
            content: vec![ContentBlock::Text { text: "ok".into() }],
            details: serde_json::json!({}),
        }).await.unwrap();

        let abort = CancellationToken::new();
        let ctx = make_tool_ctx(abort);
        tool.execute(serde_json::json!({}), &ctx).await.unwrap();

        // tool 执行期间应已落盘 descriptor
        let rec = store.load_event_wait(&task_id).await.unwrap();
        // 正常返回后 clear_event_wait 清理了 → None
        assert!(rec.is_none(), "event_wait should be cleared after normal return");
    }

    #[tokio::test]
    async fn tool_returns_stream_closed_error() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Event>(1);
        // tx drop → rx.recv() 返回 None → 流结束
        drop(tx);
        let stream: Arc<Mutex<Box<dyn EventStream>>> =
            Arc::new(Mutex::new(Box::new(ChannelStream { rx })));
        let tool = WaitForExternalEventTool::new(
            stream,
            serde_json::json!({}),
            None,
            TaskId("t1".into()),
        )
        .with_timeout(Duration::from_secs(60));

        let abort = CancellationToken::new();
        let ctx = make_tool_ctx(abort);
        let result = tool.execute(serde_json::json!({}), &ctx).await;
        assert!(matches!(result, Err(ToolError::Execution(_))));
    }
}
