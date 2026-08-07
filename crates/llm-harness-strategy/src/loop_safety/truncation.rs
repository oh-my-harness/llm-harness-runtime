//! Item 17 — truncation continuation hook.
//!
//! When the model's response is truncated by `max_tokens`, this hook continues
//! the agent loop (instead of stopping) and appends a continuation prompt
//! instructing the model to resume from the truncated tail. Continuation is
//! capped at `max_continuations` times per run.
//!
//! `should_stop` decides whether to keep looping; `transform` injects the
//! continuation user message into the context before the next LLM call. Both
//! fail open: on `try_lock` contention they stop / pass through unchanged.

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{
    AgentContext, AgentError, AgentMessage, ContentBlock, ShouldStopCtx, ShouldStopHook,
    StopReason, TransformContextCtx, TransformContextHook, UserMessage,
};
use parking_lot::Mutex;

use super::TruncationContinuationConfig;
use super::state::LoopSafetyState;

/// Hook implementing truncation continuation (item 17).
///
/// Implements `ShouldStopHook` (continue the loop on `MaxTokens` up to
/// `max_continuations` times) and `TransformContextHook` (append a
/// continuation prompt carrying the truncated tail text). Both fail open: on
/// `try_lock` contention they stop / pass the context through unchanged.
pub struct TruncationContinuationHook {
    pub(crate) state: Arc<Mutex<LoopSafetyState>>,
    pub(crate) config: TruncationContinuationConfig,
}

impl TruncationContinuationHook {
    pub(crate) fn new(
        state: Arc<Mutex<LoopSafetyState>>,
        config: TruncationContinuationConfig,
    ) -> Self {
        Self { state, config }
    }
}

impl ShouldStopHook for TruncationContinuationHook {
    fn should_stop<'a>(&'a self, ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            // Only continue on truncation; any other stop reason stops normally.
            if ctx.stop_reason != StopReason::MaxTokens {
                return true;
            }
            // Read-only check: continuation_count is incremented in transform
            // (the correct place for side effects). ShouldStopHook is a query.
            let state = match self.state.try_lock() {
                Some(g) => g,
                None => return false,
            };
            state.continuation_count >= self.config.max_continuations
        })
    }
}

impl TransformContextHook for TruncationContinuationHook {
    fn transform<'a>(
        &'a self,
        ctx: TransformContextCtx<'a>,
    ) -> BoxFuture<'a, Result<AgentContext, AgentError>> {
        Box::pin(async move {
            // Only act when the last message is a truncated assistant response.
            let Some(AgentMessage::Assistant(assistant)) = ctx.context.messages.last() else {
                return Ok(ctx.context);
            };
            if assistant.stop_reason != Some(StopReason::MaxTokens) {
                return Ok(ctx.context);
            }

            // Check continuation limit and increment atomically.
            let mut state = self.state.lock();
            if state.continuation_count >= self.config.max_continuations {
                return Ok(ctx.context);
            }
            state.continuation_count += 1;
            drop(state);

            // Extract the last text block (the truncated tail) in reverse order.
            let tail = assistant
                .content
                .iter()
                .rev()
                .find_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                });
            let Some(tail) = tail else {
                return Ok(ctx.context);
            };

            let prompt = self.config.continuation_prompt.replace("{text}", tail);
            let mut context = ctx.context;
            context.messages.push(AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text { text: prompt }],
                timestamp: chrono::Utc::now(),
            }));
            Ok(context)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{AssistantMessage, RunContext, RunRequest};
    use std::sync::LazyLock;
    use tokio_util::sync::CancellationToken;

    static RUN: LazyLock<RunContext> = LazyLock::new(|| RunContext::new(RunRequest::default()));

    fn assistant_with_text(text: &str, stop_reason: StopReason) -> AssistantMessage {
        let mut m =
            llm_harness_loop::test_utils::test_assistant_message(vec![ContentBlock::Text {
                text: text.into(),
            }]);
        m.stop_reason = Some(stop_reason);
        m
    }

    fn make_stop_ctx(assistant: &AssistantMessage) -> ShouldStopCtx<'_> {
        ShouldStopCtx {
            run: &RUN,
            last_assistant: assistant,
            stop_reason: assistant.stop_reason.unwrap_or(StopReason::EndTurn),
            turn_index: 0,
        }
    }

    fn make_transform_ctx(context: AgentContext) -> TransformContextCtx<'static> {
        TransformContextCtx {
            run: &RUN,
            turn_index: 0,
            abort: CancellationToken::new(),
            context,
        }
    }

    fn hook(max_continuations: u32, continuation_prompt: &str) -> TruncationContinuationHook {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        let config = TruncationContinuationConfig {
            max_continuations,
            continuation_prompt: continuation_prompt.to_string(),
        };
        TruncationContinuationHook::new(state, config)
    }

    // 1. MaxTokens, count=0, max=2 → should_stop=false (continue).
    #[tokio::test]
    async fn max_tokens_continues_when_under_limit() {
        let hook = hook(2, "continue from {text}");
        let msg = assistant_with_text("partial text here", StopReason::MaxTokens);
        let ctx = make_stop_ctx(&msg);
        assert!(!hook.should_stop(ctx).await);
    }

    // 2. MaxTokens, count=2, max=2 → should_stop=true (exhausted).
    #[tokio::test]
    async fn max_tokens_stops_after_max_continuations() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        {
            let mut s = state.lock();
            s.continuation_count = 2;
        }
        let config = TruncationContinuationConfig {
            max_continuations: 2,
            continuation_prompt: "continue from {text}".to_string(),
        };
        let hook = TruncationContinuationHook::new(state, config);
        let msg = assistant_with_text("partial text here", StopReason::MaxTokens);
        let ctx = make_stop_ctx(&msg);
        assert!(hook.should_stop(ctx).await);
    }

    // 3. EndTurn → should_stop=true (normal stop).
    #[tokio::test]
    async fn end_turn_stops_normally() {
        let hook = hook(2, "continue from {text}");
        let msg = assistant_with_text("done", StopReason::EndTurn);
        let ctx = make_stop_ctx(&msg);
        assert!(hook.should_stop(ctx).await);
    }

    // 4. MaxTokens assistant in messages → transform appends a UserMessage
    //    whose prompt contains "continue from" and the assistant's text.
    #[tokio::test]
    async fn transform_appends_continuation_prompt() {
        let hook = hook(2, "continue from {text}");
        let assistant = assistant_with_text("partial text here", StopReason::MaxTokens);
        let ctx = make_transform_ctx(AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::Assistant(assistant)],
        });
        let result = hook.transform(ctx).await.unwrap();
        assert_eq!(result.messages.len(), 2);
        match result.messages.last().unwrap() {
            AgentMessage::User(user) => {
                let text = match &user.content[0] {
                    ContentBlock::Text { text } => text.as_str(),
                    _ => panic!("expected text block"),
                };
                assert!(text.contains("continue from"));
                assert!(text.contains("partial text here"));
            }
            _ => panic!("expected UserMessage, got something else"),
        }
    }

    // 5. EndTurn assistant → no append, messages.len() stays 1.
    #[tokio::test]
    async fn transform_noop_on_end_turn() {
        let hook = hook(2, "continue from {text}");
        let assistant = assistant_with_text("done", StopReason::EndTurn);
        let ctx = make_transform_ctx(AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::Assistant(assistant)],
        });
        let result = hook.transform(ctx).await.unwrap();
        assert_eq!(result.messages.len(), 1);
    }

    // 6. Transform increments continuation_count.
    #[tokio::test]
    async fn transform_increments_count() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        let config = TruncationContinuationConfig {
            max_continuations: 2,
            continuation_prompt: "continue from {text}".to_string(),
        };
        let hook = TruncationContinuationHook::new(state.clone(), config);
        let assistant = assistant_with_text("partial", StopReason::MaxTokens);
        let ctx = make_transform_ctx(AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::Assistant(assistant)],
        });
        hook.transform(ctx).await.unwrap();
        assert_eq!(state.lock().continuation_count, 1);
    }

    // 7. Transform does not append when count already at max.
    #[tokio::test]
    async fn transform_noop_when_count_exhausted() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        {
            let mut s = state.lock();
            s.continuation_count = 2;
        }
        let config = TruncationContinuationConfig {
            max_continuations: 2,
            continuation_prompt: "continue from {text}".to_string(),
        };
        let hook = TruncationContinuationHook::new(state, config);
        let assistant = assistant_with_text("partial", StopReason::MaxTokens);
        let ctx = make_transform_ctx(AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::Assistant(assistant)],
        });
        let result = hook.transform(ctx).await.unwrap();
        assert_eq!(result.messages.len(), 1, "no continuation appended at max");
    }
}
