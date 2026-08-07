use std::collections::HashMap;

/// 所有 loop safety hooks 共享的可变状态，通过 `Arc<Mutex<LoopSafetyState>>` 持有。
#[derive(Default)]
pub(crate) struct LoopSafetyState {
    /// Item 15: 指纹 -> 连续出现次数。
    pub repetition_counts: HashMap<u64, u32>,
    /// Item 16: 工具名 -> 连续失败次数。
    pub failure_counts: HashMap<String, u32>,
    /// Item 17: 当前运行的 continuation 计数。
    pub continuation_count: u32,
    /// Item 18: 连续错误回合（任意 ToolFailure 自增）。
    pub consecutive_error_turns: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_empty() {
        let state = LoopSafetyState::default();
        assert!(state.repetition_counts.is_empty());
        assert!(state.failure_counts.is_empty());
        assert_eq!(state.continuation_count, 0);
        assert_eq!(state.consecutive_error_turns, 0);
    }
}
