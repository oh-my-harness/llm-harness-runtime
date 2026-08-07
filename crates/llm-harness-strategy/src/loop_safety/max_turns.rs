//! Item 19 — max turns hook.
//!
//! Enforces a hard cap on the total number of turns in a run. Once the
//! upcoming turn would meet or exceed `max_turns`, `should_stop` halts the
//! loop and `prepare` returns a hard-stop error so no further turn is
//! dispatched.
//!
//! This hook is stateless: it derives the turn count purely from
//! `ctx.turn_index`, so no shared [`LoopSafetyState`] or locking is needed.

use futures::future::BoxFuture;
use llm_harness_types::{
    AgentError, NextTurnDirective, PrepareNextTurnCtx, PrepareNextTurnHook, ShouldStopCtx,
    ShouldStopHook,
};

use super::MaxTurnsConfig;

/// Hook implementing the max-turns hard cap (item 19).
///
/// Implements `ShouldStopHook` (stop once the next turn reaches `max_turns`)
/// and `PrepareNextTurnHook` (hard-stop with an error once the limit is
/// reached). Both read `turn_index` directly from their context — no shared
/// state is required.
pub struct MaxTurnsHook {
    pub(crate) config: MaxTurnsConfig,
}

impl MaxTurnsHook {
    pub(crate) fn new(config: MaxTurnsConfig) -> Self {
        Self { config }
    }

    /// True when the upcoming turn would meet or exceed the cap.
    fn at_limit(&self, turn_index: u32) -> bool {
        turn_index + 1 >= self.config.max_turns
    }
}

impl ShouldStopHook for MaxTurnsHook {
    fn should_stop<'a>(&'a self, ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        let stop = self.at_limit(ctx.turn_index);
        Box::pin(async move { stop })
    }
}

impl PrepareNextTurnHook for MaxTurnsHook {
    fn prepare<'a>(
        &'a self,
        ctx: PrepareNextTurnCtx<'a>,
    ) -> BoxFuture<'a, Result<NextTurnDirective, AgentError>> {
        Box::pin(async move {
            if self.at_limit(ctx.turn_index) {
                return Err(AgentError::Internal(format!(
                    "max_turns limit ({}) exceeded",
                    self.config.max_turns
                )));
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

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{AssistantMessage, RunContext, RunRequest, StopReason};
    use std::sync::LazyLock;

    static RUN: LazyLock<RunContext> = LazyLock::new(|| RunContext::new(RunRequest::default()));

    static ASSISTANT: LazyLock<AssistantMessage> =
        LazyLock::new(|| llm_harness_loop::test_utils::test_assistant_message(vec![]));

    fn make_stop_ctx(turn_index: u32) -> ShouldStopCtx<'static> {
        ShouldStopCtx {
            run: &RUN,
            last_assistant: &ASSISTANT,
            stop_reason: StopReason::EndTurn,
            turn_index,
        }
    }

    fn make_prepare_ctx(turn_index: u32) -> PrepareNextTurnCtx<'static> {
        PrepareNextTurnCtx {
            run: &RUN,
            turn_index,
            last_message: &ASSISTANT,
            last_tool_results: &[],
        }
    }

    fn hook(max_turns: u32) -> MaxTurnsHook {
        MaxTurnsHook::new(MaxTurnsConfig { max_turns })
    }

    // 1. turn_index=49, max=50 → should_stop true.
    #[tokio::test]
    async fn should_stop_true_at_limit() {
        let h = hook(50);
        let ctx = make_stop_ctx(49);
        assert!(h.should_stop(ctx).await);
    }

    // 2. turn_index=10, max=50 → should_stop false.
    #[tokio::test]
    async fn should_stop_false_under_limit() {
        let h = hook(50);
        let ctx = make_stop_ctx(10);
        assert!(!h.should_stop(ctx).await);
    }

    // 3. turn_index=49, max=50 → prepare errors.
    #[tokio::test]
    async fn prepare_errors_at_limit() {
        let h = hook(50);
        let ctx = make_prepare_ctx(49);
        let res = h.prepare(ctx).await;
        let err = res.err().expect("expected error at limit");
        match err {
            AgentError::Internal(msg) => {
                assert!(msg.contains("max_turns"), "unexpected message: {msg}");
                assert!(msg.contains("50"), "unexpected message: {msg}");
            }
            other => panic!("expected AgentError::Internal, got {other:?}"),
        }
    }

    // 4. turn_index=10, max=50 → prepare Ok with all-None directive.
    #[tokio::test]
    async fn prepare_ok_under_limit() {
        let h = hook(50);
        let ctx = make_prepare_ctx(10);
        let directive = h.prepare(ctx).await.unwrap();
        assert!(directive.context.is_none());
        assert!(directive.model.is_none());
        assert!(directive.thinking_level.is_none());
        assert!(directive.temperature.is_none());
        assert!(directive.tools.is_none());
        assert!(directive.active_tools.is_none());
        assert!(directive.response_format.is_none());
    }
}
