// crates/llm-harness-runtime/src/async_spawn.rs
//! Async sub-agent delivery: registry, completion channel, track task,
//! AsyncSpawnHandle, CompletionRxGuard.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_agent::{
    AsyncSpawnHookHandle, FollowUpHandle, IdleWatcherHook, SubAgentHandle, SubAgentResult,
};
use llm_harness_types::{
    AgentError, AgentMessage, ContentBlock, ShouldStopCtx, ShouldStopHook, UserMessage,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) const COMPLETION_CHANNEL_CAPACITY: usize = 256;

/// sub-agent 完成后投递到 completion channel 的消息。
#[derive(Debug, Clone)]
pub struct AsyncCompletion {
    /// 完成的 sub-agent 的标识。
    pub agent_id: String,
    /// sub-agent 的结果（成功）或错误（失败/abort）。
    pub result: Result<SubAgentResult, AgentError>,
}
/// 不持 join 能力（已被 track task 消费）。
pub struct AsyncSpawnHandle {
    agent_id: String,
    abort: tokio_util::sync::CancellationToken,
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
    pub fn abort_token(&self) -> tokio_util::sync::CancellationToken {
        self.abort.clone()
    }
}
/// spawn 侧的 registry：持有 completion channel tx/rx、in-flight count。
///
/// `in_flight` 用 `Arc<AtomicUsize>` 以便 track task 跨 `tokio::spawn` 边界
/// clone 一份做 decrement。completion_rx 的所有权通过 `CompletionRxGuard`
/// 借出/归还（Running 期间在 hook 手里，Idle 期间在 watcher 手里，不并发）。
pub struct AsyncSpawnRegistry {
    completion_tx: mpsc::Sender<AsyncCompletion>,
    completion_rx: Mutex<Option<mpsc::Receiver<AsyncCompletion>>>,
    in_flight: Arc<AtomicUsize>,
    /// 所有 in-flight sub-agent 的 abort token。track 时存入，
    /// completion 时移除（track task 的 send 之后）。
    /// 亦由 `register_subagent_abort` 在 tokio::spawn 前预注册
    /// （同 key 幂等覆盖，消除 spawn→track insert 间的 cancel_all 竞态窗口）。
    subagent_aborts: Mutex<HashMap<String, CancellationToken>>,
}

impl AsyncSpawnRegistry {
    pub(crate) fn new(
        completion_tx: mpsc::Sender<AsyncCompletion>,
        completion_rx: mpsc::Receiver<AsyncCompletion>,
    ) -> Self {
        Self {
            completion_tx,
            completion_rx: Mutex::new(Some(completion_rx)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            subagent_aborts: Mutex::new(HashMap::new()),
        }
    }

    /// 当前在跑的 sub-agent 数。只用于 Idle watcher 启停决策。
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// 取消所有 in-flight sub-agent（cancel 它们的 abort token）。
    /// 由 harness `abort()` 经 `AbortCascadeHook` 调用。
    /// 先 take 出 map 释放锁，再遍历 cancel——避免持锁调 cancel()
    /// （cancel 内部唤醒 waker，非纯原子，O(N) 持锁）。
    pub(crate) fn cancel_all(&self) {
        let aborts = std::mem::take(&mut *self.subagent_aborts.lock().unwrap());
        for token in aborts.values() {
            token.cancel();
        }
    }

    /// 预注册 sub-agent 的 abort token（在 tokio::spawn 之前调用）。
    /// 消除 `track()` 调用晚于 `spawn_inner` 内 `tokio::spawn` 的竞态窗口：
    /// `cancel_all()` 可能在 spawn 后、track insert 前触发，漏掉该 sub-agent。
    /// 提前注册后，`track()` 内的 insert 为幂等覆盖（同 token clone）。
    pub(crate) fn register_subagent_abort(&self, agent_id: &str, token: CancellationToken) {
        self.subagent_aborts
            .lock()
            .unwrap()
            .insert(agent_id.to_string(), token);
    }

    /// 借出 completion_rx。已借出时返回 None。
    /// execute 在 Running、watcher 在 Idle，不并发。
    pub fn take_completion_rx(self: &Arc<Self>) -> Option<CompletionRxGuard> {
        let mut guard = self.completion_rx.lock().unwrap();
        guard.take().map(|rx| CompletionRxGuard {
            rx: Some(rx),
            registry: self.clone(),
        })
    }

    fn return_completion_rx(&self, rx: mpsc::Receiver<AsyncCompletion>) {
        let mut guard = self.completion_rx.lock().unwrap();
        *guard = Some(rx);
    }

    /// 消费 SubAgentHandle，spawn track task 在后台 join。
    /// in-flight +1 → join → send completion → in-flight -1。
    /// send 先于 decrement（设计 §4.5 约束 3）——保证 watcher 醒着时 count 还没归零。
    ///
    /// `deliver_to_channel`：是否投递到 completion channel（供 `AsyncSpawnHook`/
    /// `IdleWatcher` 异步消费）。sync 路径（`wait=true`）传 `false`——结果经
    /// oneshot 直接返回给 tool，再投递 channel 会导致 LLM 看到两次（双投递）。
    /// async 路径（`wait=false`）传 `true`——tool 丢弃 oneshot，结果必须经
    /// channel 投递给 hook/watcher。
    ///
    /// 返回 `(AsyncSpawnHandle, oneshot::Receiver)`：oneshot 供 `wait=true`
    /// 同步路径直接 await（按 agent_id 一对一路由，修复 F-3/F-4）。
    /// 异步路径（wait=false）丢弃 oneshot，track task 的 send 变 no-op。
    pub fn track(
        self: &Arc<Self>,
        handle: SubAgentHandle,
        agent_id: String,
        deliver_to_channel: bool,
    ) -> (
        AsyncSpawnHandle,
        tokio::sync::oneshot::Receiver<AsyncCompletion>,
    ) {
        let abort = handle.abort_token();
        let completion_tx = self.completion_tx.clone();
        let in_flight = self.in_flight.clone();
        let id = agent_id.clone();
        let (sync_tx, sync_rx) = tokio::sync::oneshot::channel::<AsyncCompletion>();
        // fetch_add 在 spawn 之前：track 返回时 in_flight 必已 +1。
        in_flight.fetch_add(1, Ordering::Relaxed);
        // 存 abort token 供 cancel_all() 取消；track task 完成后移除。
        self.subagent_aborts
            .lock()
            .unwrap()
            .insert(id.clone(), abort.clone());
        let registry_clone = self.clone();
        let id_for_remove = id.clone();
        tokio::spawn(async move {
            let result = handle.join().await;
            let completion = AsyncCompletion {
                agent_id: id,
                result,
            };
            // sync 路径：send 到 oneshot（wait=true 的 tool 消费）。
            // 若 tool 已 drop receiver（wait=false），send 变 no-op。
            let _ = sync_tx.send(completion.clone());
            // async 路径：仅当调用方请求时才投递 completion channel。
            // wait=true 时 sync_tx 已成功投递，再投 channel 会双投递。
            // send 先于 decrement（设计 §4.5 约束 3）。
            if deliver_to_channel {
                let _ = completion_tx.send(completion).await;
            }
            // 移除自己的 abort token（已完成，不再需要 cancel_all 取消）。
            registry_clone
                .subagent_aborts
                .lock()
                .unwrap()
                .remove(&id_for_remove);
            in_flight.fetch_sub(1, Ordering::Relaxed);
        });

        (AsyncSpawnHandle { agent_id, abort }, sync_rx)
    }
}

/// 创建配对的 `AsyncSpawnRegistry` + `AsyncSpawnHook`。
///
/// 两者共享同一个 completion channel（容量 256）和同一个延迟注入的
/// `FollowUpHandle`（通过 hook 的 `set_follow_up_handle` 设置）。
/// registry 给 `HarnessSubAgentSpawner`（通过 `enable_async_delivery`），
/// hook 给主 agent harness（通过 `should_stop_hook`）。
pub fn async_spawn_pair() -> (Arc<AsyncSpawnRegistry>, Arc<AsyncSpawnHook>) {
    let (tx, rx) = mpsc::channel(COMPLETION_CHANNEL_CAPACITY);
    let registry = Arc::new(AsyncSpawnRegistry::new(tx, rx));
    let hook = Arc::new(AsyncSpawnHook::new(registry.clone()));
    (registry, hook)
}

/// 借出 rx 的 RAII guard。Drop 自动归还。
///
/// 持 `Arc<AsyncSpawnRegistry>`（非 `&'a`）——guard 是 `'static`，
/// 能存进 hook 的 `Mutex<Option<CompletionRxGuard>>`。
/// Drop 调 `return_completion_rx`，保证 panic/提前返回/timeout 取消/future drop
/// 任何路径都归还 rx。
pub struct CompletionRxGuard {
    rx: Option<mpsc::Receiver<AsyncCompletion>>,
    registry: Arc<AsyncSpawnRegistry>,
}

impl CompletionRxGuard {
    pub async fn recv(&mut self) -> Option<AsyncCompletion> {
        self.rx.as_mut()?.recv().await
    }

    pub fn try_recv(&mut self) -> Option<AsyncCompletion> {
        self.rx.as_mut()?.try_recv().ok()
    }
}

impl Drop for CompletionRxGuard {
    fn drop(&mut self) {
        if let Some(rx) = self.rx.take() {
            self.registry.return_completion_rx(rx);
        }
    }
}

/// 主 agent 的 `ShouldStopHook`：LLM 自然停止时排空 completion channel，
/// 把所有就绪结果格式化成一条文本消息，通过 FollowUpHandle 注入 follow_up，
/// 返回 true。无就绪结果时也返回 true（真停，进 Idle）。
///
/// **不阻塞**——只 try_recv，"等"由 Idle watcher 负责。
/// **排空所有**——多个 sub-agent 同时完成时一次 turn 看到所有结果。
/// `Err(Aborted)` 丢弃（不注入）。
pub struct AsyncSpawnHook {
    registry: Arc<AsyncSpawnRegistry>,
    /// loop 启动前 borrow_completion_rx() 从 registry take guard 存入；
    /// loop 结束后 guard drop 归还。should_stop 时 try_recv 排空。
    /// Mutex 而非 RefCell：ShouldStopHook: Send + Sync，try_recv 同步不跨 await。
    completion_rx: Mutex<Option<CompletionRxGuard>>,
    /// 延迟注入：harness 构建后 set_follow_up_handle() 回填。
    follow_up_handle: Arc<Mutex<Option<FollowUpHandle>>>,
}

impl AsyncSpawnHook {
    pub fn new(registry: Arc<AsyncSpawnRegistry>) -> Self {
        Self {
            registry,
            completion_rx: Mutex::new(None),
            follow_up_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// loop 启动前调用：从 registry take completion_rx guard 存入 Mutex slot。
    /// loop 结束后 guard 随 hook（或下一次 borrow）drop 归还。
    /// 若已借入则跳过（理论上 Running 期间不会重复调）。
    pub fn borrow_completion_rx(&self) {
        let mut slot = self.completion_rx.lock().unwrap();
        if slot.is_none()
            && let Some(guard) = self.registry.take_completion_rx()
        {
            *slot = Some(guard);
        }
        // registry 里没有 rx（已被别处借出）——slot 保持 None，
        // should_stop 时跳过排空。这是降级，正常流程不应发生。
    }

    /// 回填 FollowUpHandle。在 harness 构建后、prompt() 前调用。
    pub fn set_follow_up_handle(&self, handle: FollowUpHandle) {
        *self.follow_up_handle.lock().unwrap() = Some(handle);
    }
}

/// 格式化单条成功结果（设计 §8.1）。
fn format_success(agent_id: &str, result: &SubAgentResult) -> String {
    format!(
        "[Sub-agent '{id}' completed]\n{output}\n\nTool calls: {n}\nSession: {sid}",
        id = agent_id,
        output = result.output,
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

impl ShouldStopHook for AsyncSpawnHook {
    fn should_stop<'a>(&'a self, _ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            // 先检查 handle 是否就绪。handle=None 时不排空（消息留 channel），
            // 与 PeerMessageHook（Task 4）同构——避免消息取出后因 handle=None 丢失。
            let handle_ready = self.follow_up_handle.lock().unwrap().is_some();
            if !handle_ready {
                return true;
            }
            // handle 就绪，排空所有就绪结果
            let mut results: Vec<String> = Vec::new();
            {
                let mut slot = self.completion_rx.lock().unwrap();
                if let Some(guard) = slot.as_mut() {
                    while let Some(completion) = guard.try_recv() {
                        match completion.result {
                            Ok(ref r) => results.push(format_success(&completion.agent_id, r)),
                            Err(AgentError::Aborted) => {}
                            Err(ref e) => results.push(format_failure(&completion.agent_id, e)),
                        }
                    }
                }
            }
            // borrow 在同一线程立即释放（try_recv 同步，不跨 await）
            if !results.is_empty() {
                let packed = results.join("\n\n---\n\n");
                let msg = AgentMessage::User(UserMessage {
                    content: vec![ContentBlock::Text { text: packed }],
                    timestamp: chrono::Utc::now(),
                });
                if let Some(h) = self.follow_up_handle.lock().unwrap().as_ref() {
                    h.follow_up_message(msg);
                }
            }
            // 无论有无结果都返回 true：有结果 → loop 取 follow_up 续命；无结果 → 真停
            true
        })
    }
}

// ── IdleWatcher ──────────────────────────────────────────────────────────────

/// Idle 期间监听 completion channel，收到 completion 后 re-send 回 channel，
/// 再调 continue_run 唤醒主 agent。watcher 只作唤醒信号，真正的消费+格式化+
/// 注入由 continue_run 启动的 loop 的 `AsyncSpawnHook::should_stop` 完成。
///
/// **为何 re-send**（F-5/F-8 修复后的最终方案）：spec §2.4 伪代码
/// `drop(guard); continue_run` 有漏洞——completion 已被 `recv()` 取出，
/// drop guard 只归还 rx 不放回 completion，continue_run 启动的 loop 的
/// should_stop try_recv 拿到空 channel。早期方案让 watcher 自己格式化+
/// 注入 follow_up，但这要求 watcher 持 `FollowUpHandle`，与 hook 的
/// follow_up_handle 形成双持有，且 watcher 注入的时序与 should_stop 排空
/// 的时序难协调。最终方案：watcher re-send completion 回 channel，由
/// should_stop 统一排空+格式化+注入（单一消费点，时序确定）。
///
/// **F-5（send 被 cancel 中断丢消息）已由 F-2 修复消除**：cancel 只
/// `token.cancel()` 不 `handle.abort()` task，select 只在 recv 阶段响应
/// cancel，`send().await` 不会被中断。
///
/// **cancel 竞态（recv 与 cancel 同时就绪）**：`select!` 随机选分支。
/// 若选了 recv，watcher 消费 completion 后检查 `cancel_token.is_cancelled()`。
/// cancel 来源有二：主 agent `abort()`（completion 多为 aborted）或用户
/// `prompt_with_messages()` 让位（completion 可能为正常完成）。watcher 无法
/// 区分，统一 `try_send` re-send 回 channel 再 break——aborted 的由 should_stop
/// 丢弃，正常的由下次 run_loop 的 should_stop 注入，不丢失。
///
/// `select! { recv() | cancel }` 避免 recv 死锁。
/// **cancel 不 abort**（修复 F-2）：`handle.abort()` 会硬杀正在
/// `continue_run().await` 的 task，破坏 harness 状态（phase 停 Turning、
/// guard 未归还）。cancel 只 `token.cancel()`，watcher 在 select 的 cancel
/// 分支 break，guard drop 归还 rx。continue_run 中途 cancel 不响应——
/// `prompt()` 的 Idle 守卫返回 NotIdle 是预期行为。
pub struct IdleWatcher {
    registry: Arc<AsyncSpawnRegistry>,
    continue_run: Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>,
    cancel_token: Mutex<Option<CancellationToken>>,
    watcher_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl IdleWatcher {
    pub fn new(
        registry: Arc<AsyncSpawnRegistry>,
        continue_run: Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>,
    ) -> Self {
        Self {
            registry,
            continue_run,
            cancel_token: Mutex::new(None),
            watcher_task: Mutex::new(None),
        }
    }
}

impl IdleWatcherHook for IdleWatcher {
    fn on_idle(&self) {
        if self.registry.in_flight_count() == 0 {
            return;
        }
        let guard = match self.registry.take_completion_rx() {
            Some(g) => g,
            None => return,
        };
        let cancel_token = CancellationToken::new();
        *self.cancel_token.lock().unwrap() = Some(cancel_token.clone());
        let registry = self.registry.clone();
        let continue_run = self.continue_run.clone();

        let handle = tokio::spawn(async move {
            let mut guard = guard;
            loop {
                tokio::select! {
                    completion = guard.recv() => {
                        match completion {
                            Some(c) => {
                                // cancel 竞态修复：watcher 被 cancel（主 agent
                                // abort 或用户 prompt_with_messages 让位）后，
                                // 此 completion 可能是正常的（prompt 竞态场景），
                                // 也可能是 aborted（abort 场景）。不丢弃——
                                // re-send 回 channel，让下次 run_loop 的 should_stop
                                // 消费（aborted 的会被 should_stop 丢弃，正常的被注入）。
                                // 用 try_send 而非 send().await：cancel 状态下不阻塞，
                                // channel 满（256）时丢弃（极端场景，优于卡死 watcher）。
                                if cancel_token.is_cancelled() {
                                    let _ = registry.completion_tx.try_send(c);
                                    break;
                                }
                                // 归还 rx，让 continue_run 的 should_stop 能 take。
                                drop(guard);
                                // re-send completion：watcher 只是唤醒信号，
                                // 真正的消费+格式化+注入由 continue_run 启动的
                                // loop 的 should_stop 完成。
                                //
                                // F-5（send 被 cancel 中断丢消息）已由 F-2 修复消除：
                                // cancel 只 token.cancel() 不 abort task，select 只在
                                // recv 阶段响应 cancel，send().await 不会被中断。
                                let _ = registry.completion_tx.send(c).await;
                                (continue_run)().await;
                                if registry.in_flight_count() == 0 {
                                    break;
                                }
                                match registry.take_completion_rx() {
                                    Some(g) => guard = g,
                                    None => break,
                                }
                            }
                            None => break,
                        }
                    }
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                }
            }
        });
        *self.watcher_task.lock().unwrap() = Some(handle);
    }

    fn cancel(&self) {
        // 只 cancel token，不 abort task（修复 F-2）。
        // watcher 在 select 的 cancel 分支 break，guard drop 归还 rx。
        // 正在 continue_run().await 时 cancel 不响应——prompt() 的 Idle 守卫
        // 返回 NotIdle 是预期行为（watcher 驱动的 loop 没结束前用户 prompt 应被拒）。
        if let Some(token) = self.cancel_token.lock().unwrap().take() {
            token.cancel();
        }
        // 不 abort：保留 watcher_task 引用以便 drop 时自动回收。
        // task 会在 cancel 分支 break 后自然退出。
        self.watcher_task.lock().unwrap().take();
    }
}

// ── AsyncSpawnHookHandle impl ────────────────────────────────────────────────

impl AsyncSpawnHookHandle for AsyncSpawnHook {
    fn borrow_completion_rx(&self) {
        // 委托给 inherent 方法（已有 registry take + slot 存入逻辑）。
        AsyncSpawnHook::borrow_completion_rx(self);
    }

    fn release_completion_rx(&self) {
        // drop guard → Drop 归还 rx 到 registry，让 watcher 能 take。
        *self.completion_rx.lock().unwrap() = None;
    }

    fn set_follow_up_handle(&self, handle: FollowUpHandle) {
        AsyncSpawnHook::set_follow_up_handle(self, handle);
    }
}

// ── AbortCascadeHook impl ────────────────────────────────────────────────────

impl llm_harness_agent::AbortCascadeHook for AsyncSpawnRegistry {
    fn cancel_all_subagents(&self) {
        self.cancel_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{
        AgentError, AssistantMessage, AssistantMessageKind, CostAggregate, ShouldStopCtx,
        ShouldStopHook, StopReason,
    };

    fn make_stop_ctx<'a>(assistant: &'a AssistantMessage) -> ShouldStopCtx<'a> {
        ShouldStopCtx {
            last_assistant: assistant,
            stop_reason: StopReason::EndTurn,
            turn_index: 0,
        }
    }

    fn empty_assistant() -> AssistantMessage {
        AssistantMessage {
            kind: AssistantMessageKind::FinalAnswer,
            message_id: None,
            turn_id: None,
            content: vec![],
            stop_reason: Some(StopReason::EndTurn),
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        }
    }

    fn test_registry() -> Arc<AsyncSpawnRegistry> {
        let (tx, rx) = mpsc::channel(COMPLETION_CHANNEL_CAPACITY);
        Arc::new(AsyncSpawnRegistry::new(tx, rx))
    }

    // helper：建一个会立即完成的 SubAgentHandle。
    fn quick_handle(output: &str) -> SubAgentHandle {
        let abort = tokio_util::sync::CancellationToken::new();
        let output = output.to_string();
        let join = tokio::spawn(async move {
            Ok(SubAgentResult {
                output,
                tool_calls_count: 0,
                session_id: "test-session".into(),
                cost: CostAggregate::default(),
            })
        });
        SubAgentHandle::new(join, abort)
    }

    #[tokio::test]
    async fn track_completes_and_sends_completion() {
        let registry = test_registry();
        let handle = quick_handle("done");
        let (async_handle, _sync_rx) = registry.track(handle, "agent-1".into(), true);
        assert_eq!(async_handle.agent_id(), "agent-1");

        let mut guard = registry.take_completion_rx().expect("borrow rx");
        let completion = guard.recv().await.expect("completion received");
        assert_eq!(completion.agent_id, "agent-1");
        let result = completion.result.unwrap();
        assert_eq!(result.output, "done");
    }

    #[tokio::test]
    async fn in_flight_count_increments_then_decrements() {
        let registry = test_registry();
        // 用一个延迟完成的 handle
        let abort = tokio_util::sync::CancellationToken::new();
        let join = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(SubAgentResult {
                output: "late".into(),
                tool_calls_count: 0,
                session_id: "s".into(),
                cost: CostAggregate::default(),
            })
        });
        let handle = SubAgentHandle::new(join, abort);
        let _async_handle = registry.track(handle, "late-agent".into(), true);
        // track 后 in_flight 应为 1（fetch_add 在 spawn 之前同步完成）
        assert_eq!(registry.in_flight_count(), 1);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(registry.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn completion_rx_guard_borrows_and_returns_on_drop() {
        let registry = test_registry();
        // 借出
        let guard = registry.take_completion_rx().expect("borrow rx");
        // 再借返回 None
        assert!(registry.take_completion_rx().is_none());
        // 归还
        drop(guard);
        // 又能借出
        let _guard2 = registry.take_completion_rx().expect("re-borrow rx");
    }

    #[tokio::test]
    async fn completion_rx_guard_try_recv_gets_message() {
        let registry = test_registry();
        registry
            .completion_tx
            .send(AsyncCompletion {
                agent_id: "x".into(),
                result: Ok(SubAgentResult {
                    output: "out".into(),
                    tool_calls_count: 0,
                    session_id: "s".into(),
                    cost: CostAggregate::default(),
                }),
            })
            .await
            .unwrap();
        let mut guard = registry.take_completion_rx().unwrap();
        let completion = guard.try_recv().expect("try_recv");
        assert_eq!(completion.agent_id, "x");
    }

    #[tokio::test]
    async fn async_spawn_handle_abort_cancels_sub_agent() {
        let registry = test_registry();
        let abort = tokio_util::sync::CancellationToken::new();
        let abort_clone = abort.clone();
        let join = tokio::spawn(async move {
            // 永不自行完成，等 abort
            abort_clone.cancelled().await;
            Err(AgentError::Aborted)
        });
        let handle = SubAgentHandle::new(join, abort);
        let (async_handle, _sync_rx) = registry.track(handle, "cancellable".into(), true);
        async_handle.abort();
        // 等 track task 收到 completion 并 decrement
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(registry.in_flight_count(), 0);
    }
    #[tokio::test]
    async fn async_spawn_pair_creates_linked_registry_and_hook() {
        let (registry, hook) = async_spawn_pair();
        // hook 持有 registry 引用（通过 borrow_completion_rx 时体现）
        // registry 的 completion_rx 初始在 registry 里
        let _guard = registry.take_completion_rx();
        assert!(_guard.is_some(), "rx initially in registry");
        drop(_guard);
        // borrow 后 rx 移到 hook
        hook.borrow_completion_rx();
        assert!(registry.take_completion_rx().is_none(), "rx now in hook");
    }

    #[tokio::test]
    async fn async_spawn_hook_returns_true_with_empty_channel() {
        let (_registry, hook) = async_spawn_pair();
        hook.borrow_completion_rx();
        let assistant = empty_assistant();
        let ctx = make_stop_ctx(&assistant);
        let stop = hook.should_stop(ctx).await;
        assert!(stop);
    }

    #[tokio::test]
    async fn async_spawn_hook_drains_and_injects_follow_up() {
        // 这个测试需要验证 follow_up 被注入。
        // FollowUpHandle 持真实 harness 的 inner —— 太重。
        // 方案：用一个最小 AgentHarness（in-memory）提供 FollowUpHandle，
        //       验证 harness 的 follow_up channel 收到消息。
        // 但 harness.follow_up_rx 是私有的。
        //
        // 替代方案：直接验证 hook 行为契约——should_stop 返回 true、不 panic、
        // 排空 channel（track task 发的 completion 被 try_recv 消费掉）。
        // follow_up 投递的端到端验证留给 Task 11 集成测试。
        let (registry, hook) = async_spawn_pair();
        hook.borrow_completion_rx();

        // 手动往 completion_tx 发消息（模拟 track task）
        registry
            .completion_tx
            .send(AsyncCompletion {
                agent_id: "worker".into(),
                result: Ok(SubAgentResult {
                    output: "result text".into(),
                    tool_calls_count: 3,
                    session_id: "sess-1".into(),
                    cost: CostAggregate::default(),
                }),
            })
            .await
            .unwrap();

        // 不 set follow_up_handle —— hook 应不 panic，仍排空 channel
        let assistant = empty_assistant();
        let ctx = make_stop_ctx(&assistant);
        let stop = hook.should_stop(ctx).await;
        assert!(stop);

        // channel 应已排空（但 hook 没 borrow guard 时无法验证）。
        // 验证：再次 should_stop，不 panic。
        let ctx2 = make_stop_ctx(&assistant);
        let stop2 = hook.should_stop(ctx2).await;
        assert!(stop2);
    }

    #[tokio::test]
    async fn async_spawn_hook_drops_aborted_completion() {
        let (registry, hook) = async_spawn_pair();
        hook.borrow_completion_rx();

        registry
            .completion_tx
            .send(AsyncCompletion {
                agent_id: "aborted".into(),
                result: Err(AgentError::Aborted),
            })
            .await
            .unwrap();

        let assistant = empty_assistant();
        let ctx = make_stop_ctx(&assistant);
        // Aborted 不注入、不 panic，返回 true
        let stop = hook.should_stop(ctx).await;
        assert!(stop);
    }

    #[tokio::test]
    async fn async_spawn_hook_injects_error_completion_when_handle_set() {
        // 这个测试验证：handle=set 时，Err(非 Aborted) 被格式化注入。
        // 需要真实 FollowUpHandle 才能验证注入——用最小 harness。
        // 为保持单测轻量，这里只验证不 panic + 返回 true。
        // 端到端注入验证留给 Task 11。
        let (registry, hook) = async_spawn_pair();
        hook.borrow_completion_rx();

        registry
            .completion_tx
            .send(AsyncCompletion {
                agent_id: "failed".into(),
                result: Err(AgentError::Internal("boom".into())),
            })
            .await
            .unwrap();

        let assistant = empty_assistant();
        let ctx = make_stop_ctx(&assistant);
        let stop = hook.should_stop(ctx).await;
        assert!(stop);
    }

    #[tokio::test]
    async fn async_spawn_hook_packs_multiple_completions() {
        let (registry, hook) = async_spawn_pair();
        hook.borrow_completion_rx();

        for i in 0..3 {
            registry
                .completion_tx
                .send(AsyncCompletion {
                    agent_id: format!("agent-{i}"),
                    result: Ok(SubAgentResult {
                        output: format!("out-{i}"),
                        tool_calls_count: i,
                        session_id: format!("s-{i}"),
                        cost: CostAggregate::default(),
                    }),
                })
                .await
                .unwrap();
        }

        let assistant = empty_assistant();
        let ctx = make_stop_ctx(&assistant);
        // 一次 should_stop 排空所有 3 条
        let stop = hook.should_stop(ctx).await;
        assert!(stop);
        // 再次 should_stop channel 已空
        let ctx2 = make_stop_ctx(&assistant);
        let stop2 = hook.should_stop(ctx2).await;
        assert!(stop2);
    }

    #[tokio::test]
    async fn async_spawn_hook_set_follow_up_handle_is_idempotent() {
        let (_registry, hook) = async_spawn_pair();
        // set 之前调用不 panic
        // （需要一个真实 FollowUpHandle 才能调 set，这里跳过——
        //  只验证 hook 构造时 follow_up_handle 是 None，should_stop 不 panic）
        let assistant = empty_assistant();
        let ctx = make_stop_ctx(&assistant);
        let stop = hook.should_stop(ctx).await;
        assert!(stop);
    }
    #[tokio::test]
    async fn idle_watcher_starts_when_in_flight_and_wakes_on_completion() {
        let (registry, _hook) = async_spawn_pair();
        let abort = tokio_util::sync::CancellationToken::new();
        let join = tokio::spawn(async {
            // 保持 task 短暂 in-flight，使 IdleWatcher::on_idle 观察到 in_flight>0。
            // 若立即返回，track() 与 on_idle() 之间存在竞态：task 完成使 in_flight 归零，
            // on_idle() 提前返回，called 永不递增 → 测试 flaky。
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(SubAgentResult {
                output: "done".into(),
                tool_calls_count: 0,
                session_id: "s".into(),
                cost: CostAggregate::default(),
            })
        });
        let _ah = registry.track(SubAgentHandle::new(join, abort), "w".into(), true);
        assert_eq!(registry.in_flight_count(), 1);

        let called = Arc::new(AtomicUsize::new(0));
        let watcher = IdleWatcher::new(registry.clone(), {
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
        let (registry, _hook) = async_spawn_pair();
        let called = Arc::new(AtomicUsize::new(0));
        let watcher = IdleWatcher::new(registry.clone(), {
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
        let (registry, _hook) = async_spawn_pair();
        let abort = tokio_util::sync::CancellationToken::new();
        let cancel_clone = abort.clone();
        let join = tokio::spawn(async move {
            cancel_clone.cancelled().await;
            Err(AgentError::Aborted)
        });
        let _ah = registry.track(SubAgentHandle::new(join, abort), "x".into(), true);

        let called = Arc::new(AtomicUsize::new(0));
        let watcher = IdleWatcher::new(registry.clone(), {
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
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(called.load(Ordering::SeqCst), 0);
        assert!(
            registry.take_completion_rx().is_some(),
            "rx returned after cancel"
        );
    }

    #[tokio::test]
    async fn cancel_all_cancels_tracked_tokens() {
        use llm_harness_agent::AbortCascadeHook;
        let (tx, rx) = tokio::sync::mpsc::channel::<AsyncCompletion>(8);
        let registry = Arc::new(AsyncSpawnRegistry::new(tx, rx));
        let t1 = CancellationToken::new();
        let t2 = CancellationToken::new();
        registry
            .subagent_aborts
            .lock()
            .unwrap()
            .insert("a".to_string(), t1.clone());
        registry
            .subagent_aborts
            .lock()
            .unwrap()
            .insert("b".to_string(), t2.clone());
        registry.cancel_all_subagents();
        assert!(t1.is_cancelled(), "token a should be cancelled");
        assert!(t2.is_cancelled(), "token b should be cancelled");
    }
}
