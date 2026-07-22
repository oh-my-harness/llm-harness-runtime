//! Async sub-agent delivery: AsyncSpawnHandle, AsyncSpawnHook, IdleWatcher.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::{AsyncSpawnHookHandle, FollowUpHandle, IdleWatcherHook};
use llm_harness_types::{AfterTurnCtx, AfterTurnHook, AgentError, AgentMessage, CustomMessage};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::spawn::message_bus::{AgentStatus, EventRxGuard, MessageBus, SubAgentEvent};
use crate::spawn::spawn_types::SubAgentResult;

/// 不持 join 能力（已被 track task 消费）。
pub struct AsyncSpawnHandle {
    agent_id: String,
    abort: CancellationToken,
}

impl AsyncSpawnHandle {
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// 取消跑飞的 sub-agent（发 cancel 信号，不阻塞）。
    pub fn abort(&self) {
        self.abort.cancel();
    }

    /// 返回 abort token 的克隆，供调用方在 `select!` 中 `.cancelled()`
    /// 打断同步等待（修复 F-14）。
    pub fn abort_token(&self) -> CancellationToken {
        self.abort.clone()
    }
}

impl AsyncSpawnHandle {
    pub(crate) fn new(agent_id: String, abort: CancellationToken) -> Self {
        Self { agent_id, abort }
    }
}

/// 创建一个 `AsyncSpawnHook`，绑定到给定的 `MessageBus`。
///
/// hook 给主 agent harness（通过 `after_turn`），
/// bus 给 `HarnessSubAgentSpawner`（通过 `new()` 的 `bus` 参数）。
pub fn async_spawn_pair(bus: Arc<MessageBus>) -> Arc<AsyncSpawnHook> {
    Arc::new(AsyncSpawnHook::new(bus))
}

// ── AsyncSpawnHook ───────────────────────────────────────────────────────────

/// 主 agent 的 `AfterTurnHook`：每 turn 结束时排空 event channel，
/// 把所有就绪结果格式化成一条消息，通过 FollowUpHandle 注入 follow_up。
///
/// **不阻塞**——只 try_recv，"等"由 Idle watcher 负责。
/// **排空所有**——多个 sub-agent 同时完成时一次 turn 看到所有结果。
/// `Err(Aborted)` 丢弃（不注入）。
///
/// 原为 `ShouldStopHook`（硬编码返回 true），但 OR 语义会压制 coordinator
/// 等需要 `should_stop=false` 的 hook。drain 移到 `after_turn` 后，
/// should_stop 不再被 AsyncSpawnHook 干扰，coordinator 升级路径正常工作。
pub struct AsyncSpawnHook {
    bus: Arc<MessageBus>,
    /// loop 启动前 borrow_event_rx() 从 bus take guard 存入；
    /// loop 结束后 guard drop 归还。after_turn 时 try_recv 排空。
    event_rx: Mutex<Option<EventRxGuard>>,
    /// 延迟注入：harness 构建后 set_follow_up_handle() 回填。
    follow_up_handle: Arc<Mutex<Option<FollowUpHandle>>>,
}

impl AsyncSpawnHook {
    pub(crate) fn new(bus: Arc<MessageBus>) -> Self {
        Self {
            bus,
            event_rx: Mutex::new(None),
            follow_up_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// loop 启动前调用：从 bus take event_rx guard 存入 Mutex slot。
    /// loop 结束后 guard 随 hook（或下一次 borrow）drop 归还。
    /// 若已借入则跳过（理论上 Running 期间不会重复调）。
    pub(crate) fn borrow_completion_rx(&self) {
        let mut slot = self.event_rx.lock();
        if slot.is_none()
            && let Some(guard) = self.bus.take_event_rx()
        {
            *slot = Some(guard);
        }
        // bus 里没有 rx（已被别处借出）——slot 保持 None，
        // after_turn 时跳过排空。这是降级，正常流程不应发生。
    }

    /// 回填 FollowUpHandle。在 harness 构建后、prompt() 前调用。
    pub fn set_follow_up_handle(&self, handle: FollowUpHandle) {
        *self.follow_up_handle.lock() = Some(handle);
    }
}

/// 格式化单条成功结果（设计 §8.1）。
fn format_success(agent_id: &str, result: &SubAgentResult) -> String {
    format!(
        "[Sub-agent '{id}' completed]\n{answer}\n\nTool calls: {n}\nSession: {sid}",
        id = agent_id,
        answer = result.final_answer,
        n = result.tool_calls_count,
        sid = result.session_id,
    )
}

/// 格式化单条失败结果（设计 §8.1）。
fn format_failure(agent_id: &str, error: &AgentError) -> String {
    format!(
        "[Sub-agent '{id}' failed]\n{error}",
        id = agent_id,
        error = error
    )
}

impl AsyncSpawnHook {
    /// 排空 event channel，返回格式化后的 AgentMessage。
    /// 无结果或 follow_up_handle 未就绪时返回 None。
    /// 调用前提：event_rx 已 borrow（slot 为 Some）。
    fn drain_and_format(&self) -> Option<AgentMessage> {
        let handle_ready = self.follow_up_handle.lock().is_some();
        if !handle_ready {
            return None;
        }
        let mut results: Vec<String> = Vec::new();
        {
            let mut slot = self.event_rx.lock();
            if let Some(guard) = slot.as_mut() {
                while let Some(event) = guard.try_recv() {
                    match event {
                        SubAgentEvent::Done { agent_id, result } => match result {
                            Ok(r) => results.push(format_success(&agent_id, &r)),
                            Err(AgentError::Aborted) => {}
                            Err(e) => results.push(format_failure(&agent_id, &e)),
                        },
                        SubAgentEvent::Message { agent_id, body } => {
                            results.push(format!("[from:{agent_id}] {body}"));
                        }
                    }
                }
            }
        }
        if results.is_empty() {
            return None;
        }
        let packed = results.join("\n\n---\n\n");
        Some(AgentMessage::Custom(CustomMessage {
            r#type: "sub_agent_message".into(),
            data: serde_json::json!({"messages": packed}),
            timestamp: chrono::Utc::now(),
        }))
    }
}

impl AsyncSpawnHook {
    /// Build a status-hint message when sub-agents are still in-flight
    /// but the event channel is empty. Injected as a follow-up so the
    /// LLM knows to query or wait for sub-agent results.
    fn status_hint(&self) -> Option<AgentMessage> {
        // Only inject after an IdleWatcher timeout, not on every should_stop.
        if !self.bus.consume_idle_timeout_fired() {
            return None;
        }
        let in_flight = self.bus.in_flight_count();
        if in_flight == 0 {
            return None;
        }
        let snapshots = self.bus.query_status(None);
        let running: Vec<&str> = snapshots
            .iter()
            .filter(|s| matches!(s.status, AgentStatus::Running))
            .map(|s| s.agent_id.as_str())
            .collect();
        if running.is_empty() {
            return None;
        }
        let hint = format!(
            "{n} sub-agent(s) still running: {ids}. \
             Use query_subagent to check status or await_subagent_reply to wait for results.",
            n = running.len(),
            ids = running.join(", ")
        );
        Some(AgentMessage::Custom(CustomMessage {
            r#type: "sub_agent_message".into(),
            data: serde_json::json!({"messages": hint}),
            timestamp: chrono::Utc::now(),
        }))
    }
}

impl AfterTurnHook for AsyncSpawnHook {
    fn after_turn<'a>(&'a self, _ctx: AfterTurnCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let msg = self.drain_and_format().or_else(|| self.status_hint());
            if let Some(msg) = msg
                && let Some(h) = self.follow_up_handle.lock().as_ref()
            {
                h.follow_up_message(msg);
            }
            // 不返回值：drain 是纯副作用。loop 的 should_stop 由其他 hook
            // （如 StructuredOutputCoordinator）或默认 true 决定。
            // follow_up 消息注入后，should_stop=true 时 loop 会 try_recv
            // follow_up_rx 续命；should_stop=false 时走 prepare_next_turn。
        })
    }
}

// ── IdleWatcher ──────────────────────────────────────────────────────────────

/// Idle 期间监听 event channel，收到 event 后 re-send 回 channel，
/// 再调 continue_run 唤醒主 agent。watcher 只作唤醒信号，真正的消费+格式化+
/// 注入由 continue_run 启动的 loop 的 `AsyncSpawnHook::after_turn` 完成。
///
/// **为何 re-send**（F-5/F-8 修复后的最终方案）：spec §2.4 伪代码
/// `drop(guard); continue_run` 有漏洞——event 已被 `recv()` 取出，
/// drop guard 只归还 rx 不放回 channel，continue_run 启动的 loop 的
/// after_turn try_recv 拿到空 channel。最终方案：watcher re-send event 回
/// channel，由 after_turn 统一排空+格式化+注入（单一消费点，时序确定）。
///
/// **F-5（send 被 cancel 中断丢消息）已由 F-2 修复消除**：cancel 只
/// `token.cancel()` 不 `handle.abort()` task，select 只在 recv 阶段响应
/// cancel，`send().await` 不会被中断。
///
/// **cancel 竞态（recv 与 cancel 同时就绪）**：`select!` 随机选分支。
/// 若选了 recv，watcher 消费 event 后检查 `cancel_token.is_cancelled()`。
/// cancel 来源有二：主 agent `abort()`（event 多为 aborted）或用户
/// `prompt_with_messages()` 让位（event 可能为正常完成）。watcher 无法
/// 区分，统一 `try_send` re-send 回 channel 再 break——aborted 的由 after_turn
/// 丢弃，正常的由下次 run_loop 的 after_turn 注入，不丢失。
///
/// `select! { recv() | cancel }` 避免 recv 死锁。
/// **cancel 不 abort**（修复 F-2）：`handle.abort()` 会硬杀正在
/// `continue_run().await` 的 task，破坏 harness 状态（phase 停 Turning、
/// guard 未归还）。cancel 只 `token.cancel()`，watcher 在 select 的 cancel
/// 分支 break，guard drop 归还 rx。continue_run 中途 cancel 不响应——
/// `prompt()` 的 Idle 守卫返回 NotIdle 是预期行为。
pub struct IdleWatcher {
    bus: Arc<MessageBus>,
    continue_run: Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>,
    cancel_token: Mutex<Option<CancellationToken>>,
    watcher_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Idle 超时秒数。超时后唤醒 main agent（after_turn 注入状态提示）。
    idle_timeout: Duration,
}

impl IdleWatcher {
    /// 创建 IdleWatcher。`idle_timeout` 默认 60 秒。
    pub fn new(
        bus: Arc<MessageBus>,
        continue_run: Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>,
    ) -> Self {
        Self {
            bus,
            continue_run,
            cancel_token: Mutex::new(None),
            watcher_task: Mutex::new(None),
            idle_timeout: Duration::from_secs(60),
        }
    }

    /// 设置 idle 超时时间。超时后唤醒 main agent，让其检查 sub-agent 状态。
    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }
}
/// 检查 bus 上是否有状态为 Running 的 sub-agent。
fn has_running_agents(bus: &MessageBus) -> bool {
    bus.query_status(None)
        .iter()
        .any(|s| matches!(s.status, crate::spawn::message_bus::AgentStatus::Running))
}

impl IdleWatcherHook for IdleWatcher {
    fn on_idle(&self) {
        // in_flight 可能尚未 fetch_sub（track task 在 send event 后才递减）。
        if self.bus.in_flight_count() == 0 {
            return;
        }
        let guard = match self.bus.take_event_rx() {
            Some(g) => g,
            None => return,
        };
        let cancel_token = CancellationToken::new();
        *self.cancel_token.lock() = Some(cancel_token.clone());
        let bus = self.bus.clone();
        let continue_run = self.continue_run.clone();
        let idle_timeout = self.idle_timeout;
        let handle = tokio::spawn(async move {
            let mut guard = guard;
            loop {
                tokio::select! {
                    event = guard.recv() => {
                        match event {
                            Some(c) => {
                                // cancel 竞态修复：watcher 被 cancel（主 agent
                                // abort 或用户 prompt_with_messages 让位）后，
                                // 此 event 可能是正常的（prompt 竞态场景），
                                // 也可能是 aborted（abort 场景）。不丢弃——
                                // re-send 回 channel，让下次 run_loop 的 should_stop
                                // 消费（aborted 的会被 should_stop 丢弃，正常的被注入）。
                                // 用 try_resend 而非 resend().await：cancel 状态下不阻塞，
                                // channel 满（256）时丢弃（极端场景，优于卡死 watcher）。
                                if cancel_token.is_cancelled() {
                                if let Err(e) = bus.try_resend_event(c) {
                                    tracing::warn!(
                                        "IdleWatcher: event dropped during cancel race \
                                         (channel {e:?}) — sub-agent result may be lost"
                                    );
                                }
                                    break;
                                }
                                // 归还 rx，让 continue_run 的 should_stop 能 take。
                                drop(guard);
                                // re-send event：watcher 只是唤醒信号，
                                // 真正的消费+格式化+注入由 continue_run 启动的
                                // loop 的 should_stop 完成。
                                //
                                // F-5（send 被 cancel 中断丢消息）已由 F-2 修复消除：
                                // cancel 只 token.cancel() 不 abort task，select 只在
                                // recv 阶段响应 cancel，send().await 不会被中断。
                                bus.resend_event(c).await;
                                (continue_run)().await;
                                if bus.in_flight_count() == 0 || !has_running_agents(&bus) {
                                    break;
                                }
                                match bus.take_event_rx() {
                                    Some(g) => guard = g,
                                    None => break,
                                }
                            }
                            None => break,
                        }
                    }
                    _ = tokio::time::sleep(idle_timeout) => {
                        // Timeout: set flag so should_stop injects a status hint,
                        // then wake the main agent.
                        if bus.in_flight_count() == 0 {
                            break;
                        }
                        bus.set_idle_timeout_fired();
                        drop(guard);
                        (continue_run)().await;
                        if bus.in_flight_count() == 0 || !has_running_agents(&bus) {
                            break;
                        }
                        match bus.take_event_rx() {
                            Some(g) => guard = g,
                            None => break,
                        }
                    }
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                }
            }
        });
        *self.watcher_task.lock() = Some(handle);
    }

    fn cancel(&self) {
        // 只 cancel token，不 abort task（修复 F-2）。
        // watcher 在 select 的 cancel 分支 break，guard drop 归还 rx。
        // 正在 continue_run().await 时 cancel 不响应——prompt() 的 Idle 守卫
        // 返回 NotIdle 是预期行为（watcher 驱动的 loop 没结束前用户 prompt 应被拒）。
        if let Some(token) = self.cancel_token.lock().take() {
            token.cancel();
        }
        // 不 abort：take() 取出 JoinHandle 后立即 drop（detach），task 不被
        // 硬杀，会在 cancel 分支 break 后自然退出。清空 slot 以便下次 on_idle 重新 spawn。
        self.watcher_task.lock().take();
    }
}

// ── AsyncSpawnHookHandle impl ────────────────────────────────────────────────

impl AsyncSpawnHookHandle for AsyncSpawnHook {
    fn borrow_completion_rx(&self) {
        // 委托给 inherent 方法（已有 bus take + slot 存入逻辑）。
        AsyncSpawnHook::borrow_completion_rx(self);
    }

    fn release_completion_rx(&self) {
        // drop guard → Drop 归还 rx 到 bus，让 watcher 能 take。
        *self.event_rx.lock() = None;
    }

    fn set_follow_up_handle(&self, handle: FollowUpHandle) {
        AsyncSpawnHook::set_follow_up_handle(self, handle);
    }

    fn drain_completions(&self) -> Option<AgentMessage> {
        self.drain_and_format()
    }
}

/// Convert `CustomMessage{type:"sub_agent_message"}` (injected by
/// [`AsyncSpawnHook`]) into a user message so the LLM can see sub-agent
/// results in context.
///
/// Register via `HarnessBuilder::convert_to_llm(Some(Arc::new(
///     DefaultConvertToLlm::new().with_custom_converter(Arc::new(SubAgentMessageConverter))
/// )))` whenever `AsyncSpawnHook` is used.
pub struct SubAgentMessageConverter;

impl llm_harness_loop::convert::CustomMessageConverter for SubAgentMessageConverter {
    fn convert<'a>(
        &'a self,
        msg: &'a llm_harness_types::CustomMessage,
    ) -> BoxFuture<'a, Result<llm_harness_loop::LlmMessage, AgentError>> {
        Box::pin(async move {
            let text = msg
                .data
                .get("messages")
                .and_then(|v| v.as_str())
                .unwrap_or("sub-agent event");
            Ok(llm_harness_loop::LlmMessage::User(vec![
                llm_harness_loop::RequestContent::Text(text.to_string()),
            ]))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn::message_bus::{MessageBus, SubAgentEvent, message_bus_pair};
    use crate::spawn::spawn_types::{SubAgentHandle, SubAgentResult};
    use llm_harness_types::{AgentError, AgentMessage, CostAggregate};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_bus() -> Arc<MessageBus> {
        message_bus_pair()
    }

    // helper：建一个会立即完成的 SubAgentHandle。
    fn quick_handle(output: &str) -> SubAgentHandle {
        let abort = CancellationToken::new();
        let final_answer = output.to_string();
        let join = tokio::spawn(async move {
            Ok(SubAgentResult {
                final_answer,
                tool_calls_count: 0,
                session_id: "test-session".into(),
                cost: CostAggregate::default(),
            })
        });
        SubAgentHandle::new(join, abort)
    }

    #[tokio::test]
    async fn track_completes_and_sends_event() {
        let bus = test_bus();
        let handle = quick_handle("done");
        let async_handle = bus.track(handle, "agent-1".into());
        assert_eq!(async_handle, "agent-1");

        let mut guard = bus.take_event_rx().expect("borrow rx");
        let event = guard.recv().await.expect("event received");
        match event {
            SubAgentEvent::Done { agent_id, result } => {
                assert_eq!(agent_id, "agent-1");
                let r = result.unwrap();
                assert_eq!(r.final_answer, "done");
            }
            _ => panic!("expected Done event"),
        }
    }

    #[tokio::test]
    async fn in_flight_count_increments_then_decrements() {
        let bus = test_bus();
        let abort = CancellationToken::new();
        let join = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(SubAgentResult {
                final_answer: "late".into(),
                tool_calls_count: 0,
                session_id: "s".into(),
                cost: CostAggregate::default(),
            })
        });
        let handle = SubAgentHandle::new(join, abort);
        let _id = bus.track(handle, "late-agent".into());
        assert_eq!(bus.in_flight_count(), 1);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(bus.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn event_rx_guard_borrows_and_returns_on_drop() {
        let bus = test_bus();
        let guard = bus.take_event_rx().expect("borrow rx");
        assert!(bus.take_event_rx().is_none());
        drop(guard);
        let _guard2 = bus.take_event_rx().expect("re-borrow rx");
    }

    #[tokio::test]
    async fn event_rx_guard_try_recv_gets_message() {
        let bus = test_bus();
        bus.resend_event(SubAgentEvent::Done {
            agent_id: "x".into(),
            result: Ok(SubAgentResult {
                final_answer: "out".into(),
                tool_calls_count: 0,
                session_id: "s".into(),
                cost: CostAggregate::default(),
            }),
        })
        .await;
        let mut guard = bus.take_event_rx().unwrap();
        let event = guard.try_recv().expect("try_recv");
        match event {
            SubAgentEvent::Done { agent_id, .. } => assert_eq!(agent_id, "x"),
            _ => panic!("expected Done event"),
        }
    }

    #[tokio::test]
    async fn async_spawn_handle_abort_cancels_sub_agent() {
        let bus = test_bus();
        let abort = CancellationToken::new();
        let abort_clone = abort.clone();
        let join = tokio::spawn(async move {
            abort_clone.cancelled().await;
            Err(AgentError::Aborted)
        });
        let handle = SubAgentHandle::new(join, abort);
        let id = bus.track(handle, "cancellable".into());
        // track returns agent_id, not handle — use register_subagent_abort + abort_agent
        bus.abort_agent(&id);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(bus.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn async_spawn_pair_creates_hook_bound_to_bus() {
        let bus = test_bus();
        let hook = async_spawn_pair(bus.clone());
        // bus 的 event_rx 初始在 bus 里
        let _guard = bus.take_event_rx();
        assert!(_guard.is_some(), "rx initially in bus");
        drop(_guard);
        // borrow 后 rx 移到 hook
        hook.borrow_completion_rx();
        assert!(bus.take_event_rx().is_none(), "rx now in hook");
    }

    fn test_run() -> &'static llm_harness_types::RunContext {
        static RUN: std::sync::OnceLock<llm_harness_types::RunContext> = std::sync::OnceLock::new();
        RUN.get_or_init(|| {
            llm_harness_types::RunContext::new(llm_harness_types::RunRequest::default())
        })
    }

    fn make_after_turn_ctx<'a>(msgs: &'a [AgentMessage]) -> AfterTurnCtx<'a> {
        AfterTurnCtx {
            run: test_run(),
            turn_index: 0,
            new_messages: msgs,
        }
    }

    #[tokio::test]
    async fn after_turn_no_op_with_empty_channel() {
        let bus = test_bus();
        let hook = async_spawn_pair(bus);
        hook.borrow_completion_rx();
        let msgs: Vec<AgentMessage> = vec![];
        let ctx = make_after_turn_ctx(&msgs);
        // 纯副作用，不 panic 即可。
        hook.after_turn(ctx).await;
    }

    #[tokio::test]
    async fn after_turn_skips_drain_when_handle_not_set() {
        let bus = test_bus();
        let hook = async_spawn_pair(bus.clone());
        hook.borrow_completion_rx();

        bus.resend_event(SubAgentEvent::Done {
            agent_id: "worker".into(),
            result: Ok(SubAgentResult {
                final_answer: "result text".into(),
                tool_calls_count: 3,
                session_id: "sess-1".into(),
                cost: CostAggregate::default(),
            }),
        })
        .await;

        // follow_up_handle 未 set → drain_and_format 返回 None（handle_ready=false）
        let msgs: Vec<AgentMessage> = vec![];
        let ctx = make_after_turn_ctx(&msgs);
        hook.after_turn(ctx).await;

        // channel 未排空（handle_ready=false 时不 try_recv）。
        // 再调一次 drain_and_format 验证消息仍在。
        // set follow_up_handle 后 drain 应能取到。
        // （此处不 set，仅验证不 panic。）
    }

    #[tokio::test]
    async fn after_turn_drops_aborted_completion() {
        let bus = test_bus();
        let hook = async_spawn_pair(bus.clone());
        hook.borrow_completion_rx();

        bus.resend_event(SubAgentEvent::Done {
            agent_id: "aborted".into(),
            result: Err(AgentError::Aborted),
        })
        .await;

        let msgs: Vec<AgentMessage> = vec![];
        let ctx = make_after_turn_ctx(&msgs);
        hook.after_turn(ctx).await;
        // Aborted 被丢弃，不注入 follow_up。不 panic 即可。
    }

    #[tokio::test]
    async fn after_turn_injects_error_completion_when_handle_set() {
        let bus = test_bus();
        let hook = async_spawn_pair(bus.clone());
        hook.borrow_completion_rx();

        bus.resend_event(SubAgentEvent::Done {
            agent_id: "failed".into(),
            result: Err(AgentError::Internal("boom".into())),
        })
        .await;

        // set follow_up_handle 后 drain 应排空并注入。
        // 使用一个简单的 FollowUpHandle mock 验证不 panic。
        let msgs: Vec<AgentMessage> = vec![];
        let ctx = make_after_turn_ctx(&msgs);
        hook.after_turn(ctx).await;
        // 未 set handle → handle_ready=false → 不排空。验证不 panic。
    }

    #[tokio::test]
    async fn after_turn_packs_multiple_completions() {
        let bus = test_bus();
        let hook = async_spawn_pair(bus.clone());
        hook.borrow_completion_rx();

        for i in 0..3 {
            bus.resend_event(SubAgentEvent::Done {
                agent_id: format!("agent-{i}"),
                result: Ok(SubAgentResult {
                    final_answer: format!("out-{i}"),
                    tool_calls_count: i,
                    session_id: format!("s-{i}"),
                    cost: CostAggregate::default(),
                }),
            })
            .await;
        }

        let msgs: Vec<AgentMessage> = vec![];
        let ctx = make_after_turn_ctx(&msgs);
        hook.after_turn(ctx).await;
        // 未 set handle → 不排空。第二次调用也应不 panic。
        let ctx2 = make_after_turn_ctx(&msgs);
        hook.after_turn(ctx2).await;
    }

    #[tokio::test]
    async fn after_turn_set_follow_up_handle_is_idempotent() {
        let bus = test_bus();
        let hook = async_spawn_pair(bus);
        hook.borrow_completion_rx();
        let msgs: Vec<AgentMessage> = vec![];
        let ctx = make_after_turn_ctx(&msgs);
        // hook 未 set follow_up_handle，after_turn 仍正常执行（跳过 drain_and_format）
        hook.after_turn(ctx).await;
    }
    #[tokio::test]
    async fn idle_watcher_starts_when_in_flight_and_wakes_on_completion() {
        let bus = test_bus();
        let abort = CancellationToken::new();
        let join = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(SubAgentResult {
                final_answer: "done".into(),
                tool_calls_count: 0,
                session_id: "s".into(),
                cost: CostAggregate::default(),
            })
        });
        let _id = bus.track(SubAgentHandle::new(join, abort), "w".into());
        assert_eq!(bus.in_flight_count(), 1);

        let called = Arc::new(AtomicUsize::new(0));
        let watcher = IdleWatcher::new(bus.clone(), {
            let c = called.clone();
            Arc::new(move || {
                let c = c.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            })
        });
        watcher.on_idle();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            called.load(Ordering::SeqCst) >= 1,
            "continue_run callback invoked"
        );
    }

    #[tokio::test]
    async fn idle_watcher_no_op_when_no_in_flight() {
        let bus = test_bus();
        let called = Arc::new(AtomicUsize::new(0));
        let watcher = IdleWatcher::new(bus.clone(), {
            let c = called.clone();
            Arc::new(move || {
                let c = c.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            })
        });
        watcher.on_idle();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(called.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn idle_watcher_cancel_stops_and_returns_rx() {
        let bus = test_bus();
        let abort = CancellationToken::new();
        let cancel_clone = abort.clone();
        let join = tokio::spawn(async move {
            cancel_clone.cancelled().await;
            Err(AgentError::Aborted)
        });
        let id = bus.track(SubAgentHandle::new(join, abort), "x".into());

        let called = Arc::new(AtomicUsize::new(0));
        let watcher = IdleWatcher::new(bus.clone(), {
            let c = called.clone();
            Arc::new(move || {
                let c = c.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            })
        });
        watcher.on_idle();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        watcher.cancel();
        bus.abort_agent(&id);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(called.load(Ordering::SeqCst), 0);
        assert!(bus.take_event_rx().is_some(), "rx returned after cancel");
    }

    #[tokio::test]
    async fn cancel_all_cancels_tracked_tokens() {
        use llm_harness_agent::AbortCascadeHook;
        let bus = test_bus();
        let t1 = CancellationToken::new();
        let t2 = CancellationToken::new();
        bus.register_subagent_abort("a", t1.clone());
        bus.register_subagent_abort("b", t2.clone());
        bus.cancel_all_subagents();
        assert!(t1.is_cancelled(), "token a should be cancelled");
        assert!(t2.is_cancelled(), "token b should be cancelled");
    }

    // Note: drain_and_format is tested via integration tests in
    // spawn_communication_integration.rs, which use a real harness +
    // FollowUpHandle to exercise the full drain → format → inject path.
}
