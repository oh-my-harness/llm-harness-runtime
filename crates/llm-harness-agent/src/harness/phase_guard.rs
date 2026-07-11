use llm_harness_types::{HarnessError, HarnessPhase};
use tokio_util::sync::CancellationToken;

use super::AgentHarness;
use super::event::AgentHarnessEvent;

// ── PhaseGuard ────────────────────────────────────────────────────────────────

/// RAII guard：进入非 Idle phase，离开作用域时自动恢复 Idle。
///
/// 构造时在一次锁内完成"检查当前为 Idle → 设为目标 phase"，
/// 消除 check-then-set 的 TOCTOU 窗口。失败返回 `NotIdle`。
/// Drop 时无条件恢复为 Idle 并发射 `PhaseChange` 事件。
pub(super) struct PhaseGuard<'a> {
    harness: &'a AgentHarness,
}

impl std::fmt::Debug for PhaseGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhaseGuard").finish_non_exhaustive()
    }
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        self.harness.set_phase(HarnessPhase::Idle);
    }
}

// ── AbortTokenGuard ──────────────────────────────────────────────────────────

/// RAII guard: temporarily replaces `current_abort` with a new token,
/// restoring the previous value on drop (even on panic).
pub(super) struct AbortTokenGuard<'a> {
    harness: &'a AgentHarness,
    prev: Option<CancellationToken>,
}

impl AbortTokenGuard<'_> {
    pub(super) fn new(harness: &AgentHarness, token: CancellationToken) -> AbortTokenGuard<'_> {
        let prev = {
            let mut inner = harness.inner.lock().unwrap_or_else(|e| e.into_inner());
            let prev = inner.current_abort.take();
            inner.current_abort = Some(token);
            prev
        };
        AbortTokenGuard { harness, prev }
    }
}

impl Drop for AbortTokenGuard<'_> {
    fn drop(&mut self) {
        let mut inner = self.harness.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.current_abort = self.prev.take();
    }
}

// ── Phase management methods ─────────────────────────────────────────────────

impl AgentHarness {
    pub(super) fn notify_phase_change(&self, old: HarnessPhase, new: HarnessPhase) {
        let _ = self.phase_tx.send(new);
        self.emit(AgentHarnessEvent::PhaseChange { from: old, to: new });
    }

    pub(super) fn set_phase(&self, phase: HarnessPhase) {
        let old = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let old = inner.state.phase;
            inner.state.phase = phase;
            old
        };
        self.notify_phase_change(old, phase);
    }

    /// 原子地检查 Idle 并切换到 `phase`。失败返回 Err。
    pub(super) fn enter_phase(&self, phase: HarnessPhase) -> Result<PhaseGuard<'_>, HarnessError> {
        let old = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
            inner.state.phase = phase;
            HarnessPhase::Idle
        };
        self.notify_phase_change(old, phase);
        Ok(PhaseGuard { harness: self })
    }
}
