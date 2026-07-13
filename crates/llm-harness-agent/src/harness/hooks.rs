use llm_harness_types::AgentMessage;

use super::FollowUpHandle;

// ── Idle watcher / async spawn hook decoupling traits ────────────────────────

/// Idle watcher 解耦 trait（由 runtime crate 的 `IdleWatcher` 实现）。
/// harness 在 `run_loop` 结束进入 Idle 后调 `on_idle`；在 `prompt_with_messages`
/// 守卫通过后调 `cancel`。watcher 持 continue_run 回调而非 harness 引用。
pub trait IdleWatcherHook: Send + Sync {
    /// loop 结束进入 Idle 后调用。检查 in-flight count，>0 则启动 watcher。
    fn on_idle(&self);
    /// prompt() 启动前调用：取消已有 watcher，让其归还 completion_rx。
    fn cancel(&self);
}

/// AsyncSpawnHook 解耦 trait（由 runtime crate 的 `AsyncSpawnHook` 实现）。
/// harness 在 `run_loop` 开头 borrow、结尾 release completion_rx guard。
pub trait AsyncSpawnHookHandle: Send + Sync {
    /// loop 启动前：从 registry 借出 completion_rx guard 存入 hook。
    fn borrow_completion_rx(&self);
    /// loop 结束后：drop guard，归还 rx 到 registry，让 watcher 能 take。
    fn release_completion_rx(&self);
    /// 回填 `FollowUpHandle`。由 `set_async_spawn_hook` 自动调用，
    /// 保证 should_stop 能注入 follow_up（修复 F-6：生产中 follow_up_handle
    /// 从未 set 导致 should_stop 永远走 handle_ready=false 死循环）。
    fn set_follow_up_handle(&self, handle: FollowUpHandle);
    /// 排空 completion channel，返回格式化后的消息。
    /// 必须在 `borrow_completion_rx` 之后调用。
    /// 用于 `continue_run` 在启动 `run_loop` 前预排空，避免浪费一轮 LLM 调用。
    fn drain_completions(&self) -> Option<AgentMessage>;
}

/// Abort 级联 hook（由 `MessageBus` 实现）。
/// harness `abort()` 时调用，取消所有通过本 harness 派发的 in-flight sub-agent。
pub trait AbortCascadeHook: Send + Sync {
    /// 取消所有通过本 harness 派发的 in-flight sub-agent。
    ///
    /// 调用时机：`AgentHarness::abort()` 中，cancel idle watcher 之后、
    /// cancel 自身 abort token 之前。
    /// 非阻塞：只 cancel token（置标志），不 join sub-agent task。
    /// 幂等：对已 cancel 的 token 再调 cancel 无效；空 registry 无操作。
    /// 生命周期持久：abort 不终止 agent 生命周期（可 re-prompt），
    /// hook 应可重复调用（实现方不应 take/consume 自身）。
    fn cancel_all_subagents(&self);
}
