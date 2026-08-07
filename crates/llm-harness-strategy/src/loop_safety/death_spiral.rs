//! Item 18 — death spiral guard hook.
//!
//! Tracks the number of consecutive turns that produced any tool failure
//! (`consecutive_error_turns` on the shared [`LoopSafetyState`]). A turn with
//! no failures resets the counter to zero. Once the counter reaches
//! `max_error_turns`, `should_stop` halts the loop to avoid an unproductive
//! error spiral.
//!
//! Turn aggregation uses `PrepareNextTurnHook` (called once per turn boundary)
//! instead of `AfterToolCallHook` (called per tool, concurrently in parallel
//! batches) to avoid non-deterministic counter updates.

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{
    AgentError, NextTurnDirective, PrepareNextTurnCtx, PrepareNextTurnHook, ShouldStopCtx,
    ShouldStopHook,
};
use parking_lot::Mutex;

use super::DeathSpiralGuardConfig;
use super::state::LoopSafetyState;

/// Hook implementing the death spiral guard (item 18).
///
/// Implements `PrepareNextTurnHook` (increment if any tool failed this turn,
/// reset on all-success) and `ShouldStopHook` (stop once
/// `consecutive_error_turns >= max_error_turns`). `prepare` uses blocking
/// `lock()` (await-free critical section); `should_stop` fails open on
/// contention.
pub struct DeathSpiralGuardHook {
    pub(crate) state: Arc<Mutex<LoopSafetyState>>,
    pub(crate) config: DeathSpiralGuardConfig,
}

impl DeathSpiralGuardHook {
    pub(crate) fn new(state: Arc<Mutex<LoopSafetyState>>, config: DeathSpiralGuardConfig) -> Self {
        Self { state, config }
    }
}

impl PrepareNextTurnHook for DeathSpiralGuardHook {
    fn prepare<'a>(
        &'a self,
        ctx: PrepareNextTurnCtx<'a>,
    ) -> BoxFuture<'a, Result<NextTurnDirective, AgentError>> {
        Box::pin(async move {
            let had_error = ctx.last_tool_results.iter().any(|(_, r)| r.is_err());
            let mut state = self.state.lock();
            if had_error {
                state.consecutive_error_turns += 1;
            } else {
                state.consecutive_error_turns = 0;
            }
            Ok(NextTurnDirective {
                context: None,
                model: None,
                thinking_level: None,
                temperature: None,
                tools: None,
                active_tools: None,
                response_format: None,
            })
        })
    }
}

impl ShouldStopHook for DeathSpiralGuardHook {
    fn should_stop<'a>(&'a self, _ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let state = match self.state.try_lock() {
                Some(g) => g,
                None => return false,
            };
            state.consecutive_error_turns >= self.config.max_error_turns
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{
        AssistantMessage, PrepareNextTurnCtx, RunContext, RunRequest, ShouldStopCtx, StopReason,
        ToolFailure, ToolResult,
    };
    use std::sync::LazyLock;

    static RUN: LazyLock<RunContext> = LazyLock::new(|| RunContext::new(RunRequest::default()));
    static ASSISTANT: LazyLock<AssistantMessage> =
        LazyLock::new(|| llm_harness_loop::test_utils::test_assistant_message(vec![]));

    fn make_prepare_ctx(
        results: &[(String, Result<ToolResult, ToolFailure>)],
    ) -> PrepareNextTurnCtx<'_> {
        PrepareNextTurnCtx {
            run: &RUN,
            turn_index: 0,
            last_message: &ASSISTANT,
            last_tool_results: results,
        }
    }

    fn make_stop_ctx() -> ShouldStopCtx<'static> {
        ShouldStopCtx {
            run: &RUN,
            last_assistant: &ASSISTANT,
            stop_reason: StopReason::EndTurn,
            turn_index: 0,
        }
    }

    fn ok_result() -> (String, Result<ToolResult, ToolFailure>) {
        (
            "tu_1".to_string(),
            Ok(ToolResult::full(vec![], serde_json::json!({}), false)),
        )
    }

    fn err_result() -> (String, Result<ToolResult, ToolFailure>) {
        (
            "tu_1".to_string(),
            Err(ToolFailure::new("test_error", "something failed")),
        )
    }

    // 1. Turn with an error → consecutive_error_turns becomes 1.
    #[tokio::test]
    async fn increments_on_error_turn() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        let config = DeathSpiralGuardConfig::default();
        let hook = DeathSpiralGuardHook::new(state.clone(), config);
        let results = [err_result()];
        let ctx = make_prepare_ctx(&results);
        hook.prepare(ctx).await.unwrap();
        assert_eq!(state.lock().consecutive_error_turns, 1);
    }

    // 2. Pre-set to 3, all-success turn → reset to 0.
    #[tokio::test]
    async fn resets_on_success_turn() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        {
            let mut s = state.lock();
            s.consecutive_error_turns = 3;
        }
        let config = DeathSpiralGuardConfig::default();
        let hook = DeathSpiralGuardHook::new(state.clone(), config);
        let results = [ok_result()];
        let ctx = make_prepare_ctx(&results);
        hook.prepare(ctx).await.unwrap();
        assert_eq!(state.lock().consecutive_error_turns, 0);
    }

    // 3. Mixed turn (one ok + one err) → increments (had error).
    #[tokio::test]
    async fn mixed_turn_counts_as_one_error() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        let config = DeathSpiralGuardConfig::default();
        let hook = DeathSpiralGuardHook::new(state.clone(), config);
        let results = [ok_result(), err_result()];
        let ctx = make_prepare_ctx(&results);
        hook.prepare(ctx).await.unwrap();
        assert_eq!(state.lock().consecutive_error_turns, 1);
    }

    // 4. Pre-set 5, max=5 → should_stop true.
    #[tokio::test]
    async fn should_stop_when_exceeded() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        {
            let mut s = state.lock();
            s.consecutive_error_turns = 5;
        }
        let config = DeathSpiralGuardConfig { max_error_turns: 5 };
        let hook = DeathSpiralGuardHook::new(state, config);
        assert!(hook.should_stop(make_stop_ctx()).await);
    }

    // 5. Pre-set 2, max=5 → should_stop false.
    #[tokio::test]
    async fn should_not_stop_when_under_limit() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        {
            let mut s = state.lock();
            s.consecutive_error_turns = 2;
        }
        let config = DeathSpiralGuardConfig { max_error_turns: 5 };
        let hook = DeathSpiralGuardHook::new(state, config);
        assert!(!hook.should_stop(make_stop_ctx()).await);
    }

    // 6. Empty results (non-tool turn) → reset to 0.
    #[tokio::test]
    async fn empty_results_resets() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        {
            let mut s = state.lock();
            s.consecutive_error_turns = 3;
        }
        let config = DeathSpiralGuardConfig::default();
        let hook = DeathSpiralGuardHook::new(state.clone(), config);
        let ctx = make_prepare_ctx(&[]);
        hook.prepare(ctx).await.unwrap();
        assert_eq!(state.lock().consecutive_error_turns, 0);
    }
}
