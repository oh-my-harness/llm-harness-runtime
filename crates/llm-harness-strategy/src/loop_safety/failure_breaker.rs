//! Item 16 — failure circuit breaker hook.
//!
//! Tracks per-tool consecutive failures. After a tool fails
//! `max_consecutive_failures` times in a row, subsequent calls to that tool
//! are denied with a `ToolFailure` whose `model_message` reports the tool
//! name and observed failure count. A successful call resets the counter to
//! zero. `should_stop` halts the loop once any tool's counter reaches the
//! limit.
//!
//! All three hook entry points fail open: on `try_lock` contention they
//! allow the call / pass through the result / decline to stop.

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{
    AfterToolCallCtx, AfterToolCallDecision, AfterToolCallHook, BeforeToolCallCtx,
    BeforeToolCallDecision, BeforeToolCallHook, ShouldStopCtx, ShouldStopHook, ToolFailure,
};
use parking_lot::Mutex;

use super::FailureCircuitBreakerConfig;
use super::state::LoopSafetyState;

/// Hook implementing the failure circuit breaker (item 16).
///
/// Implements `AfterToolCallHook` (increment on `Err`, reset on `Ok`),
/// `BeforeToolCallHook` (deny once the counter reaches the limit), and
/// `ShouldStopHook` (stop the loop once any tool's counter reaches the
/// limit). All three fail open: on `try_lock` contention they allow the
/// call / pass through the result / decline to stop.
pub struct FailureCircuitBreakerHook {
    pub(crate) state: Arc<Mutex<LoopSafetyState>>,
    pub(crate) config: FailureCircuitBreakerConfig,
}

impl FailureCircuitBreakerHook {
    pub(crate) fn new(
        state: Arc<Mutex<LoopSafetyState>>,
        config: FailureCircuitBreakerConfig,
    ) -> Self {
        Self { state, config }
    }
}

impl AfterToolCallHook for FailureCircuitBreakerHook {
    fn on_complete<'a>(
        &'a self,
        ctx: AfterToolCallCtx<'a>,
    ) -> BoxFuture<'a, AfterToolCallDecision> {
        Box::pin(async move {
            // Blocking lock: critical section is nanosecond-level (no await),
            // safe in async context. Prevents parallel-batch try_lock failures
            // from skipping failure count updates.
            let mut state = self.state.lock();
            match ctx.result {
                Err(_) => {
                    *state
                        .failure_counts
                        .entry(ctx.tool_name.to_string())
                        .or_insert(0) += 1;
                }
                Ok(_) => {
                    state.failure_counts.insert(ctx.tool_name.to_string(), 0);
                }
            }
            AfterToolCallDecision::Passthrough
        })
    }
}

impl BeforeToolCallHook for FailureCircuitBreakerHook {
    fn on_call<'a>(&'a self, ctx: BeforeToolCallCtx<'a>) -> BoxFuture<'a, BeforeToolCallDecision> {
        Box::pin(async move {
            // Fail open: if the lock is held, allow the call.
            let state = match self.state.try_lock() {
                Some(g) => g,
                None => return BeforeToolCallDecision::Allow,
            };
            let count = state
                .failure_counts
                .get(ctx.tool_name)
                .copied()
                .unwrap_or(0);
            drop(state);

            if count >= self.config.max_consecutive_failures {
                let model_message = format!(
                    "{}: tool={}, failures={}",
                    self.config.deny_message, ctx.tool_name, count
                );
                BeforeToolCallDecision::Deny(ToolFailure::new("circuit_broken", model_message))
            } else {
                BeforeToolCallDecision::Allow
            }
        })
    }
}

impl ShouldStopHook for FailureCircuitBreakerHook {
    fn should_stop<'a>(&'a self, _ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            // Fail open: on contention, do not stop.
            let state = match self.state.try_lock() {
                Some(g) => g,
                None => return false,
            };
            state
                .failure_counts
                .values()
                .any(|&c| c >= self.config.max_consecutive_failures)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{
        AssistantMessage, BeforeToolCallCtx, BeforeToolCallDecision, RunContext, RunRequest,
        ShouldStopCtx, StopReason, ToolResult,
    };
    use std::sync::LazyLock;

    static RUN: LazyLock<RunContext> = LazyLock::new(|| RunContext::new(RunRequest::default()));

    fn make_before_ctx<'a>(
        assistant: &'a AssistantMessage,
        tool_name: &'a str,
        args: &'a serde_json::Value,
    ) -> BeforeToolCallCtx<'a> {
        BeforeToolCallCtx {
            run: &RUN,
            assistant_message: assistant,
            tool_use_id: "tu_1",
            tool_name,
            args,
            turn_index: 0,
        }
    }

    fn make_after_ctx<'a>(
        assistant: &'a AssistantMessage,
        tool_name: &'a str,
        result: &'a Result<ToolResult, ToolFailure>,
    ) -> AfterToolCallCtx<'a> {
        AfterToolCallCtx {
            run: &RUN,
            assistant_message: assistant,
            tool_use_id: "tu_1",
            tool_name,
            args: &serde_json::Value::Null,
            result,
            turn_index: 0,
        }
    }

    fn make_stop_ctx(assistant: &AssistantMessage) -> ShouldStopCtx<'_> {
        ShouldStopCtx {
            run: &RUN,
            last_assistant: assistant,
            stop_reason: StopReason::EndTurn,
            turn_index: 0,
        }
    }

    fn assistant() -> AssistantMessage {
        llm_harness_loop::test_utils::test_assistant_message(vec![])
    }

    #[allow(clippy::result_large_err)]
    fn ok_result() -> Result<ToolResult, ToolFailure> {
        Ok(ToolResult::full(vec![], serde_json::json!({}), false))
    }

    #[allow(clippy::result_large_err)]
    fn err_result() -> Result<ToolResult, ToolFailure> {
        Err(ToolFailure::new("test_error", "something failed"))
    }

    fn hook(max_consecutive_failures: u32) -> FailureCircuitBreakerHook {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        let config = FailureCircuitBreakerConfig {
            max_consecutive_failures,
            deny_message: "circuit broken".to_string(),
        };
        FailureCircuitBreakerHook::new(state, config)
    }

    // 1. Empty state → Allow.
    #[tokio::test]
    async fn allows_when_no_failures() {
        let hook = hook(3);
        let msg = assistant();
        let args = serde_json::json!({"command": "ls"});
        let ctx = make_before_ctx(&msg, "bash", &args);
        let d = hook.on_call(ctx).await;
        assert!(matches!(d, BeforeToolCallDecision::Allow));
    }

    // 2. Pre-set failures at the limit → Deny.
    #[tokio::test]
    async fn denies_after_max_consecutive_failures() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        {
            let mut s = state.lock();
            s.failure_counts.insert("bash".to_string(), 3);
        }
        let config = FailureCircuitBreakerConfig {
            max_consecutive_failures: 3,
            deny_message: "circuit broken".to_string(),
        };
        let hook = FailureCircuitBreakerHook::new(state, config);
        let msg = assistant();
        let args = serde_json::json!({"command": "ls"});
        let ctx = make_before_ctx(&msg, "bash", &args);
        match hook.on_call(ctx).await {
            BeforeToolCallDecision::Deny(tf) => {
                assert_eq!(tf.code, "circuit_broken");
                assert!(tf.model_message.contains("bash"));
                assert!(tf.model_message.contains("failures=3"));
            }
            _ => panic!("expected Deny, got non-Deny decision"),
        }
    }

    // 3. Err result → counter increments to 1.
    #[tokio::test]
    async fn after_hook_increments_failure_on_error() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        let config = FailureCircuitBreakerConfig::default();
        let hook = FailureCircuitBreakerHook::new(state.clone(), config);
        let msg = assistant();
        let result = err_result();
        let ctx = make_after_ctx(&msg, "bash", &result);
        let d = hook.on_complete(ctx).await;
        assert!(matches!(d, AfterToolCallDecision::Passthrough));
        assert_eq!(*state.lock().failure_counts.get("bash").unwrap(), 1);
    }

    // 4. Pre-set count, Ok result → reset to 0.
    #[tokio::test]
    async fn after_hook_resets_on_success() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        {
            let mut s = state.lock();
            s.failure_counts.insert("bash".to_string(), 2);
        }
        let config = FailureCircuitBreakerConfig::default();
        let hook = FailureCircuitBreakerHook::new(state.clone(), config);
        let msg = assistant();
        let result = ok_result();
        let ctx = make_after_ctx(&msg, "bash", &result);
        let d = hook.on_complete(ctx).await;
        assert!(matches!(d, AfterToolCallDecision::Passthrough));
        assert_eq!(*state.lock().failure_counts.get("bash").unwrap(), 0);
    }

    // 5. Counter above the limit → should_stop true.
    #[tokio::test]
    async fn should_stop_true_when_circuit_open() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        {
            let mut s = state.lock();
            s.failure_counts.insert("bash".to_string(), 5);
        }
        let config = FailureCircuitBreakerConfig {
            max_consecutive_failures: 3,
            deny_message: "circuit broken".to_string(),
        };
        let hook = FailureCircuitBreakerHook::new(state, config);
        let msg = assistant();
        let ctx = make_stop_ctx(&msg);
        assert!(hook.should_stop(ctx).await);
    }

    // 6. Empty state → should_stop false.
    #[tokio::test]
    async fn should_stop_false_when_no_circuit() {
        let hook = hook(3);
        let msg = assistant();
        let ctx = make_stop_ctx(&msg);
        assert!(!hook.should_stop(ctx).await);
    }
}
