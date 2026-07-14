use std::sync::Mutex;

use llm_harness_types::AgentMessage;

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
    /// 与既有 `AgentHarness::follow_up_message`（`harness.rs:963`）语义一致：
    /// 正常路径不会失败——should_stop 返回 true 后 loop 在 `loop_fn.rs:519`
    /// 立即 `try_recv` 取走，`reset_channels` 在下一次 `prompt()` 才调。
    ///
    /// 持 `Weak` 而非 `Arc`：打断 `harness.inner → async_spawn_hook/peer_hook
    /// → FollowUpHandle → harness.inner` 的引用环。harness drop 后 upgrade
    /// 失败，静默 no-op（hook 的宿主已不存在）。
    pub fn follow_up_message(&self, msg: AgentMessage) {
        if let Some(inner) = self.inner.upgrade() {
            let inner = inner.lock().unwrap();
            let _ = inner.follow_up_tx.try_send(msg);
        }
    }
}
