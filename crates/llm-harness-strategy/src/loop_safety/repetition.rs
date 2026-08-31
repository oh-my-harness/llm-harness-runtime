//! Item 15 — repetition guard hook.
//!
//! Detects repeated identical tool calls by fingerprinting
//! `tool_name + canonical_json(args)`. After `max_repetitions` identical
//! calls, subsequent identical calls are denied with a `ToolFailure` whose
//! `model_message` reports the tool name and observed count.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{
    BeforeToolCallCtx, BeforeToolCallDecision, BeforeToolCallHook, ShouldStopCtx, ShouldStopHook,
    ToolFailure,
};
use parking_lot::Mutex;

use super::RepetitionGuardConfig;
use super::state::LoopSafetyState;

/// Recursively produce a deterministic JSON string: object keys are sorted,
/// arrays preserve order, scalars use `serde_json::Value::to_string`.
pub(crate) fn canonicalize_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            // serde_json's Map preserves insertion order; sort by key for determinism.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = String::from("{");
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).expect("string key"));
                out.push(':');
                out.push_str(&canonicalize_json(&map[*k]));
            }
            out.push('}');
            out
        }
        serde_json::Value::Array(arr) => {
            let mut out = String::from("[");
            for (i, v) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&canonicalize_json(v));
            }
            out.push(']');
            out
        }
        // Scalars (and Null) — `to_string` already produces canonical JSON.
        _ => value.to_string(),
    }
}

/// Compute a non-cryptographic fingerprint of a tool call.
///
/// Hashes `tool_name + ":" + canonical_json(args)` with `DefaultHasher`.
/// Same `(tool_name, args)` → same `u64`; key-order differences in `args`
/// do not change the fingerprint.
pub(crate) fn compute_fingerprint(tool_name: &str, args: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(tool_name.as_bytes());
    hasher.write(b":");
    let canon = canonicalize_json(args);
    hasher.write(canon.as_bytes());
    hasher.finish()
}

/// Hook implementing the repetition guard (item 15).
///
/// Implements both `BeforeToolCallHook` (deny after `max_repetitions`
/// identical calls) and `ShouldStopHook` (stop the loop once any
/// fingerprint has exceeded the limit). Both fail open: on `try_lock`
/// contention they allow / continue.
pub struct RepetitionGuardHook {
    pub(crate) state: Arc<Mutex<LoopSafetyState>>,
    pub(crate) config: RepetitionGuardConfig,
}

impl RepetitionGuardHook {
    pub(crate) fn new(state: Arc<Mutex<LoopSafetyState>>, config: RepetitionGuardConfig) -> Self {
        Self { state, config }
    }
}

impl BeforeToolCallHook for RepetitionGuardHook {
    fn on_call<'a>(&'a self, ctx: BeforeToolCallCtx<'a>) -> BoxFuture<'a, BeforeToolCallDecision> {
        Box::pin(async move {
            let fp = compute_fingerprint(ctx.tool_name, ctx.args);
            // Fail open: if the lock is held, allow the call rather than
            // blocking the loop.
            let mut state = match self.state.try_lock() {
                Some(g) => g,
                None => return BeforeToolCallDecision::Allow,
            };
            let count = state.repetition_counts.entry(fp).or_insert(0);
            *count += 1;
            let count = *count;
            drop(state);

            if count > self.config.max_repetitions {
                let model_message = format!(
                    "{}: tool={}, count={}",
                    self.config.deny_message, ctx.tool_name, count
                );
                BeforeToolCallDecision::Deny(ToolFailure::denied(
                    "repetition_detected",
                    model_message,
                ))
            } else {
                BeforeToolCallDecision::Allow
            }
        })
    }
}

impl ShouldStopHook for RepetitionGuardHook {
    fn should_stop<'a>(&'a self, _ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            // Fail open: on contention, do not stop.
            let state = match self.state.try_lock() {
                Some(g) => g,
                None => return false,
            };
            state
                .repetition_counts
                .values()
                .any(|&c| c > self.config.max_repetitions)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{
        AssistantMessage, BeforeToolCallCtx, BeforeToolCallDecision, RunContext, RunRequest,
        ShouldStopCtx, StopReason,
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

    fn hook(max_repetitions: u32) -> RepetitionGuardHook {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        let config = RepetitionGuardConfig {
            max_repetitions,
            deny_message: "repetition detected".to_string(),
        };
        RepetitionGuardHook::new(state, config)
    }

    // 1. Determinism: same input → same fingerprint.
    #[test]
    fn fingerprint_is_deterministic() {
        let args = serde_json::json!({"path": "src/main.rs", "limit": 10});
        let a = compute_fingerprint("read", &args);
        let b = compute_fingerprint("read", &args);
        assert_eq!(a, b);
    }

    // 2. Key order independence.
    #[test]
    fn fingerprint_ignores_key_order() {
        let a = compute_fingerprint("write", &serde_json::json!({"a": 1, "b": 2}));
        let b = compute_fingerprint("write", &serde_json::json!({"b": 2, "a": 1}));
        assert_eq!(a, b);
    }

    // 3. Different args → different fingerprint.
    #[test]
    fn fingerprint_differs_for_different_args() {
        let a = compute_fingerprint("read", &serde_json::json!({"path": "a.rs"}));
        let b = compute_fingerprint("read", &serde_json::json!({"path": "b.rs"}));
        assert_ne!(a, b);
    }

    // 4. Allow first N, then deny.
    #[tokio::test]
    async fn allows_first_n_calls_then_denies() {
        let hook = hook(3);
        let msg = assistant();
        let args = serde_json::json!({"command": "ls"});
        // Calls 1–3: Allow.
        for _ in 0..3 {
            let ctx = make_before_ctx(&msg, "bash", &args);
            let d = hook.on_call(ctx).await;
            assert!(
                matches!(d, BeforeToolCallDecision::Allow),
                "calls 1–3 allow"
            );
        }
        // Call 4: Deny.
        let ctx = make_before_ctx(&msg, "bash", &args);
        let d = hook.on_call(ctx).await;
        match d {
            BeforeToolCallDecision::Deny(tf) => {
                assert_eq!(tf.code, "repetition_detected");
                assert!(tf.model_message.contains("bash"));
                assert!(tf.model_message.contains("count=4"));
            }
            _ => panic!("expected Deny, got non-Deny decision"),
        }
    }

    // 5. Different args → independent fingerprint counting → Allow.
    #[tokio::test]
    async fn different_args_reset_repetition() {
        let hook = hook(2);
        let msg = assistant();
        let args_a = serde_json::json!({"path": "a.rs"});
        let args_b = serde_json::json!({"path": "b.rs"});
        // Two identical calls of A hit the limit (count 2 == max, still Allow).
        for _ in 0..2 {
            let ctx = make_before_ctx(&msg, "read", &args_a);
            let d = hook.on_call(ctx).await;
            assert!(matches!(d, BeforeToolCallDecision::Allow));
        }
        // A third A would be denied (count 3 > 2); but a different fingerprint
        // (B) starts its own counter → Allow.
        let ctx = make_before_ctx(&msg, "read", &args_b);
        let d = hook.on_call(ctx).await;
        assert!(
            matches!(d, BeforeToolCallDecision::Allow),
            "different args → Allow"
        );
    }

    // 6. should_stop true when any fingerprint exceeds the limit.
    #[tokio::test]
    async fn should_stop_true_when_repetition_exceeded() {
        let state = Arc::new(Mutex::new(LoopSafetyState::default()));
        {
            let mut s = state.lock();
            s.repetition_counts.insert(42, 5);
        }
        let config = RepetitionGuardConfig {
            max_repetitions: 3,
            deny_message: "repetition detected".to_string(),
        };
        let hook = RepetitionGuardHook::new(state, config);
        let msg = assistant();
        let ctx = make_stop_ctx(&msg);
        assert!(hook.should_stop(ctx).await);
    }

    // 7. should_stop false when nothing exceeded.
    #[tokio::test]
    async fn should_stop_false_when_no_repetition() {
        let hook = hook(3);
        let msg = assistant();
        let ctx = make_stop_ctx(&msg);
        assert!(!hook.should_stop(ctx).await);
    }
}
