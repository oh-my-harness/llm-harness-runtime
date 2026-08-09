use std::sync::Mutex;

use llm_harness_types::{AgentMessage, ChannelError};

use super::state::HarnessInner;

// ── FollowUpHandle ────────────────────────────────────────────────────────────

/// 对 `AgentHarness` follow_up channel 的可克隆句柄。
///
/// 持有与 `AgentHarness` 同一个 `Arc<Mutex<HarnessInner>>`，使外部组件
/// （`AsyncSpawnHook`、`IdleWatcher`）能在不持有整个
/// harness 引用的前提下注入 follow_up 消息。
///
/// **不缓存** `follow_up_tx`——每次 `follow_up_message` 都重新 `lock()` 取
/// 当前 sender，保证 `reset_channels()` 替换 sender 后总能拿到最新的。
/// 持 `std::sync::Mutex` 期间不跨 await（`try_send` 非阻塞）。
#[derive(Clone)]
pub struct FollowUpHandle {
    pub(super) inner: std::sync::Weak<Mutex<HarnessInner>>,
}

impl FollowUpHandle {
    /// 注入一条 follow_up 消息到 harness 当前的 follow_up channel。
    ///
    /// 返回 `Err(ChannelError::Closed)` 当 harness 已 drop（Weak upgrade 失败）
    /// 或 harness 处于 Idle 阶段（channel 被 `reset_channels` 替换）。
    /// 返回 `Err(ChannelError::Full)` 表示队列已满。
    ///
    /// 持 `Weak` 而非 `Arc`：打断 `harness.inner → async_spawn_hook/peer_hook
    /// → FollowUpHandle → harness.inner` 的引用环。harness drop 后 upgrade
    /// 失败，返回 `Err(Closed)`。
    pub fn follow_up_message(&self, msg: AgentMessage) -> Result<(), ChannelError> {
        let inner = self.inner.upgrade().ok_or(ChannelError::Closed)?;
        let inner = inner.lock().unwrap();
        inner.follow_up_tx.try_send(msg).map_err(ChannelError::from)
    }
}
