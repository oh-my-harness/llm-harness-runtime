use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{
    AfterProviderResponseHook, AfterToolCallCtx, AfterToolCallDecision, AfterToolCallHook,
    AfterTurnCtx, AfterTurnHook, AgentContext, AgentError, AuthHook, AuthInfo, BeforeCompactCtx,
    BeforeCompactDecision, BeforeCompactHook, BeforeProviderRequestHook, BeforeRunCtx,
    BeforeRunHook, BeforeRunResult, BeforeToolCallCtx, BeforeToolCallDecision, BeforeToolCallHook,
    BeforeTurnCtx, BeforeTurnHook, PrepareNextTurnCtx, PrepareNextTurnHook, ProviderResponseInfo,
    ShouldStopCtx, ShouldStopHook, StreamOptions, TransformContextHook,
};

/// Chains multiple `AfterProviderResponseHook` implementations (all-execute semantics).
///
/// All hooks execute in registration order. Return value: `()` (observing only).
pub struct CompositeAfterProviderResponseHook {
    hooks: Vec<Arc<dyn AfterProviderResponseHook>>,
}

impl CompositeAfterProviderResponseHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn AfterProviderResponseHook>>) -> Self {
        Self { hooks }
    }
}

impl AfterProviderResponseHook for CompositeAfterProviderResponseHook {
    fn after_response<'a>(&'a self, info: &'a ProviderResponseInfo) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.hooks {
                hook.after_response(info).await;
            }
        })
    }
}

/// Chains multiple `AfterToolCallHook` implementations (last-Patch-wins semantics).
///
/// All hooks execute in order, each seeing the original (unpatched) result.
/// If multiple hooks return `Patch`, the last one wins; earlier patches are
/// discarded. A `Passthrough` from any hook does not reset a prior `Patch`.
pub struct CompositeAfterToolCallHook {
    hooks: Vec<Arc<dyn AfterToolCallHook>>,
}

impl CompositeAfterToolCallHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn AfterToolCallHook>>) -> Self {
        Self { hooks }
    }
}

impl AfterToolCallHook for CompositeAfterToolCallHook {
    fn on_complete<'a>(
        &'a self,
        ctx: AfterToolCallCtx<'a>,
    ) -> BoxFuture<'a, AfterToolCallDecision> {
        Box::pin(async move {
            let mut final_decision = AfterToolCallDecision::Passthrough;

            for hook in &self.hooks {
                // Each hook sees the original, unpatched result.
                let hook_ctx = AfterToolCallCtx {
                    assistant_message: ctx.assistant_message,
                    tool_use_id: ctx.tool_use_id,
                    tool_name: ctx.tool_name,
                    args: ctx.args,
                    result: ctx.result,
                    turn_index: ctx.turn_index,
                };

                let decision = hook.on_complete(hook_ctx).await;

                if let AfterToolCallDecision::Patch(_) = decision {
                    final_decision = decision;
                }
            }

            final_decision
        })
    }
}

/// Chains multiple `BeforeToolCallHook` implementations (first-non-allow semantics).
///
/// Hooks execute in order; the first `Deny` or `Modify` decision short-circuits.
pub struct CompositeBeforeToolCallHook {
    hooks: Vec<Arc<dyn BeforeToolCallHook>>,
}

impl CompositeBeforeToolCallHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn BeforeToolCallHook>>) -> Self {
        Self { hooks }
    }
}

impl BeforeToolCallHook for CompositeBeforeToolCallHook {
    fn on_call<'a>(&'a self, ctx: BeforeToolCallCtx<'a>) -> BoxFuture<'a, BeforeToolCallDecision> {
        Box::pin(async move {
            for hook in &self.hooks {
                // Reconstruct ctx for each hook (BeforeToolCallCtx is not Clone).
                let hook_ctx = BeforeToolCallCtx {
                    assistant_message: ctx.assistant_message,
                    tool_use_id: ctx.tool_use_id,
                    tool_name: ctx.tool_name,
                    args: ctx.args,
                    turn_index: ctx.turn_index,
                };
                let decision = hook.on_call(hook_ctx).await;
                if !matches!(decision, BeforeToolCallDecision::Allow) {
                    return decision; // short-circuit on first non-allow
                }
            }
            BeforeToolCallDecision::Allow
        })
    }
}

/// Chains multiple `AfterTurnHook` implementations (all-execute semantics).
///
/// All hooks execute in registration order (observing only, no return value).
pub struct CompositeAfterTurnHook {
    hooks: Vec<Arc<dyn AfterTurnHook>>,
}

impl CompositeAfterTurnHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn AfterTurnHook>>) -> Self {
        Self { hooks }
    }
}

impl AfterTurnHook for CompositeAfterTurnHook {
    fn after_turn<'a>(&'a self, ctx: AfterTurnCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.hooks {
                let hook_ctx = AfterTurnCtx {
                    turn_index: ctx.turn_index,
                    new_messages: ctx.new_messages,
                };
                hook.after_turn(hook_ctx).await;
            }
        })
    }
}

/// Chains multiple `BeforeProviderRequestHook` implementations (all-execute semantics).
///
/// All hooks execute in registration order, each may mutate `StreamOptions`.
pub struct CompositeBeforeProviderRequestHook {
    hooks: Vec<Arc<dyn BeforeProviderRequestHook>>,
}

impl CompositeBeforeProviderRequestHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn BeforeProviderRequestHook>>) -> Self {
        Self { hooks }
    }
}

impl BeforeProviderRequestHook for CompositeBeforeProviderRequestHook {
    fn before_request<'a>(&'a self, opts: &'a mut StreamOptions) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.hooks {
                hook.before_request(opts).await;
            }
        })
    }
}

/// Chains multiple `BeforeRunHook` implementations (all-execute semantics).
///
/// All hooks execute in registration order. Additional messages accumulate;
/// system_prompt from the last non-None result wins.
pub struct CompositeBeforeRunHook {
    hooks: Vec<Arc<dyn BeforeRunHook>>,
}

impl CompositeBeforeRunHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn BeforeRunHook>>) -> Self {
        Self { hooks }
    }
}

impl BeforeRunHook for CompositeBeforeRunHook {
    fn before_run<'a>(
        &'a self,
        ctx: BeforeRunCtx<'a>,
    ) -> BoxFuture<'a, Result<BeforeRunResult, AgentError>> {
        Box::pin(async move {
            let mut accumulated_messages = Vec::new();
            let mut system_prompt_override = None;

            for hook in &self.hooks {
                let result = hook
                    .before_run(BeforeRunCtx {
                        prompt_text: ctx.prompt_text,
                        initial_messages: ctx.initial_messages,
                        system_prompt: ctx.system_prompt,
                        resources: ctx.resources,
                    })
                    .await?;

                accumulated_messages.extend(result.additional_messages);
                if let Some(sp) = result.system_prompt {
                    system_prompt_override = Some(sp);
                }
            }

            Ok(BeforeRunResult {
                additional_messages: accumulated_messages,
                system_prompt: system_prompt_override,
            })
        })
    }
}

/// Chains multiple `BeforeTurnHook` implementations (all-execute semantics).
///
/// All hooks execute in registration order (observing only, no return value).
pub struct CompositeBeforeTurnHook {
    hooks: Vec<Arc<dyn BeforeTurnHook>>,
}

impl CompositeBeforeTurnHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn BeforeTurnHook>>) -> Self {
        Self { hooks }
    }
}

impl BeforeTurnHook for CompositeBeforeTurnHook {
    fn before_turn<'a>(&'a self, ctx: BeforeTurnCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.hooks {
                let hook_ctx = BeforeTurnCtx {
                    turn_index: ctx.turn_index,
                    snapshot: ctx.snapshot,
                };
                hook.before_turn(hook_ctx).await;
            }
        })
    }
}

/// Chains multiple `TransformContextHook` implementations (chain semantics).
///
/// Each hook sees the result from the previous hook's transformation.
pub struct CompositeTransformContextHook {
    hooks: Vec<Arc<dyn TransformContextHook>>,
}

impl CompositeTransformContextHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn TransformContextHook>>) -> Self {
        Self { hooks }
    }
}

impl TransformContextHook for CompositeTransformContextHook {
    fn transform<'a>(
        &'a self,
        mut ctx: AgentContext,
    ) -> BoxFuture<'a, Result<AgentContext, AgentError>> {
        Box::pin(async move {
            for hook in &self.hooks {
                ctx = hook.transform(ctx).await?;
            }
            Ok(ctx)
        })
    }
}

/// Chains multiple `PrepareNextTurnHook` implementations (field-merge semantics).
///
/// All hooks execute in order. Each field takes the last non-None value.
pub struct CompositePrepareNextTurnHook {
    hooks: Vec<Arc<dyn PrepareNextTurnHook>>,
}

impl CompositePrepareNextTurnHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn PrepareNextTurnHook>>) -> Self {
        Self { hooks }
    }
}

impl PrepareNextTurnHook for CompositePrepareNextTurnHook {
    fn prepare<'a>(
        &'a self,
        ctx: PrepareNextTurnCtx<'a>,
    ) -> BoxFuture<'a, Result<llm_harness_types::NextTurnDirective, AgentError>> {
        Box::pin(async move {
            let mut directive = llm_harness_types::NextTurnDirective {
                context: None,
                model: None,
                thinking_level: None,
                temperature: None,
                tools: None,
                active_tools: None,
            };

            for hook in &self.hooks {
                let result = hook
                    .prepare(PrepareNextTurnCtx {
                        turn_index: ctx.turn_index,
                        last_message: ctx.last_message,
                        last_tool_results: ctx.last_tool_results,
                    })
                    .await?;
                if result.context.is_some() {
                    directive.context = result.context;
                }
                if result.model.is_some() {
                    directive.model = result.model;
                }
                if result.thinking_level.is_some() {
                    directive.thinking_level = result.thinking_level;
                }
                if result.temperature.is_some() {
                    directive.temperature = result.temperature;
                }
                if result.tools.is_some() {
                    directive.tools = result.tools;
                }
                if result.active_tools.is_some() {
                    directive.active_tools = result.active_tools;
                }
            }

            Ok(directive)
        })
    }
}

/// Chains multiple `ShouldStopHook` implementations (all-execute semantics).
///
/// All hooks execute in registration order; returns `true` if any hook returns
/// `true`. Unlike short-circuit composites, every hook always runs — this is
/// deliberate: `ShouldStopHook` is a query (not a decision gate), and hooks
/// may carry side effects such as draining a follow-up channel
/// (`AsyncSpawnHook`, `PeerMessageHook`) that must not be skipped just because
/// an earlier hook (e.g. `BudgetControlAdapter`) already returned `true`.
/// Short-circuiting would silently defer or drop pending follow-up messages.
pub struct CompositeShouldStopHook {
    hooks: Vec<Arc<dyn ShouldStopHook>>,
}

impl CompositeShouldStopHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn ShouldStopHook>>) -> Self {
        Self { hooks }
    }
}

impl ShouldStopHook for CompositeShouldStopHook {
    fn should_stop<'a>(&'a self, ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let mut any_true = false;
            for hook in &self.hooks {
                let hook_ctx = ShouldStopCtx {
                    last_assistant: ctx.last_assistant,
                    stop_reason: ctx.stop_reason,
                    turn_index: ctx.turn_index,
                };
                if hook.should_stop(hook_ctx).await {
                    any_true = true;
                }
            }
            any_true
        })
    }
}

/// Chains multiple `BeforeCompactHook` implementations (first-non-proceed short-circuit semantics).
///
/// Hooks execute in order; the first `Skip` or `Override` decision short-circuits.
pub struct CompositeBeforeCompactHook {
    hooks: Vec<Arc<dyn BeforeCompactHook>>,
}

impl CompositeBeforeCompactHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn BeforeCompactHook>>) -> Self {
        Self { hooks }
    }
}

impl BeforeCompactHook for CompositeBeforeCompactHook {
    fn before_compact<'a>(
        &'a self,
        ctx: BeforeCompactCtx<'a>,
    ) -> BoxFuture<'a, BeforeCompactDecision> {
        Box::pin(async move {
            for hook in &self.hooks {
                let hook_ctx = BeforeCompactCtx {
                    estimated_tokens: ctx.estimated_tokens,
                    messages: ctx.messages,
                    context_window: ctx.context_window,
                    reserve_tokens: ctx.reserve_tokens,
                    keep_recent_tokens: ctx.keep_recent_tokens,
                };
                let decision = hook.before_compact(hook_ctx).await;
                if !matches!(decision, BeforeCompactDecision::Proceed) {
                    return decision; // short-circuit on first non-proceed
                }
            }
            BeforeCompactDecision::Proceed
        })
    }
}

/// Chains multiple `AuthHook` implementations (last-wins semantics).
///
/// Hooks execute in order; the last non-error result wins.
pub struct CompositeAuthHook {
    hooks: Vec<Arc<dyn AuthHook>>,
}

impl CompositeAuthHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn AuthHook>>) -> Self {
        Self { hooks }
    }
}

impl AuthHook for CompositeAuthHook {
    fn resolve<'a>(&'a self) -> BoxFuture<'a, Result<AuthInfo, AgentError>> {
        Box::pin(async move {
            let mut last_result: Option<Result<AuthInfo, AgentError>> = None;
            for hook in &self.hooks {
                match hook.resolve().await {
                    Ok(info) => last_result = Some(Ok(info)),
                    // Don't clobber an earlier success with a later error:
                    // "last non-error result wins" semantics means we keep
                    // trying remaining hooks but never overwrite a valid
                    // credential with a failure from a lower-priority source.
                    Err(_) if last_result.is_some() => {}
                    Err(e) => last_result = Some(Err(e)),
                }
            }
            last_result.unwrap_or(Ok(AuthInfo {
                api_key: None,
                headers: vec![],
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_after_provider_response_hook_is_object_safe() {
        fn _accept(_: &dyn AfterProviderResponseHook) {}
        let composite = CompositeAfterProviderResponseHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_after_tool_call_hook_is_object_safe() {
        fn _accept(_: &dyn AfterToolCallHook) {}
        let composite = CompositeAfterToolCallHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_before_tool_call_hook_is_object_safe() {
        fn _accept(_: &dyn BeforeToolCallHook) {}
        let composite = CompositeBeforeToolCallHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_after_turn_hook_is_object_safe() {
        fn _accept(_: &dyn AfterTurnHook) {}
        let composite = CompositeAfterTurnHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_before_provider_request_hook_is_object_safe() {
        fn _accept(_: &dyn BeforeProviderRequestHook) {}
        let composite = CompositeBeforeProviderRequestHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_before_run_hook_is_object_safe() {
        fn _accept(_: &dyn BeforeRunHook) {}
        let composite = CompositeBeforeRunHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_before_turn_hook_is_object_safe() {
        fn _accept(_: &dyn BeforeTurnHook) {}
        let composite = CompositeBeforeTurnHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_transform_context_hook_is_object_safe() {
        fn _accept(_: &dyn TransformContextHook) {}
        let composite = CompositeTransformContextHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_prepare_next_turn_hook_is_object_safe() {
        fn _accept(_: &dyn PrepareNextTurnHook) {}
        let composite = CompositePrepareNextTurnHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_should_stop_hook_is_object_safe() {
        fn _accept(_: &dyn ShouldStopHook) {}
        let composite = CompositeShouldStopHook::new(vec![]);
        _accept(&composite);
    }

    #[tokio::test]
    async fn composite_should_stop_hook_runs_all_hooks_despite_early_true() {
        // Regression: CompositeShouldStopHook must NOT short-circuit.
        // An earlier hook returning true (e.g. BudgetControlAdapter on
        // budget exceeded) must not skip a later follow-up-injecting hook
        // (e.g. AsyncSpawnHook), otherwise pending sub-agent results would
        // be silently deferred. All hooks execute; result is the OR.
        use llm_harness_types::{ContentBlock, StopReason};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Flagger {
            flag: Arc<AtomicBool>,
            ret: bool,
        }
        impl ShouldStopHook for Flagger {
            fn should_stop<'a>(&'a self, _ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
                Box::pin(async move {
                    self.flag.store(true, Ordering::SeqCst);
                    self.ret
                })
            }
        }

        let first_ran = Arc::new(AtomicBool::new(false));
        let second_ran = Arc::new(AtomicBool::new(false));

        let composite = CompositeShouldStopHook::new(vec![
            Arc::new(Flagger {
                flag: first_ran.clone(),
                ret: true,
            }),
            Arc::new(Flagger {
                flag: second_ran.clone(),
                ret: false,
            }),
        ]);

        let assistant = {
            let mut m = crate::test_utils::test_assistant_message(vec![ContentBlock::Text {
                text: "x".into(),
            }]);
            m.stop_reason = Some(StopReason::EndTurn);
            m
        };
        let ctx = ShouldStopCtx {
            last_assistant: &assistant,
            stop_reason: StopReason::EndTurn,
            turn_index: 0,
        };

        let result = composite.should_stop(ctx).await;
        assert!(result, "OR of [true, false] must be true");
        assert!(first_ran.load(Ordering::SeqCst), "first hook must run");
        assert!(
            second_ran.load(Ordering::SeqCst),
            "second hook must run despite first returning true"
        );
    }

    #[test]
    fn composite_before_compact_hook_is_object_safe() {
        fn _accept(_: &dyn BeforeCompactHook) {}
        let composite = CompositeBeforeCompactHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_auth_hook_is_object_safe() {
        fn _accept(_: &dyn AuthHook) {}
        let composite = CompositeAuthHook::new(vec![]);
        _accept(&composite);
    }

    #[tokio::test]
    async fn composite_auth_hook_keeps_first_success_on_later_error() {
        // Regression: "last non-error result wins" — a later hook erroring
        // must not clobber an earlier success.
        use llm_harness_types::{AgentError, AuthInfo};

        struct StaticKey(&'static str);
        impl AuthHook for StaticKey {
            fn resolve<'a>(&'a self) -> BoxFuture<'a, Result<AuthInfo, AgentError>> {
                Box::pin(async move {
                    Ok(AuthInfo {
                        api_key: Some(self.0.into()),
                        headers: vec![],
                    })
                })
            }
        }

        struct Failing;
        impl AuthHook for Failing {
            fn resolve<'a>(&'a self) -> BoxFuture<'a, Result<AuthInfo, AgentError>> {
                Box::pin(async move { Err(AgentError::Internal("oauth refresh failed".into())) })
            }
        }

        // [Ok, Err] → Ok preserved
        let composite =
            CompositeAuthHook::new(vec![Arc::new(StaticKey("sk-first")), Arc::new(Failing)]);
        let result = composite.resolve().await;
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().api_key.as_deref(),
            Some("sk-first"),
            "earlier success must survive later error"
        );

        // [Err, Ok] → Ok from second hook wins
        let composite =
            CompositeAuthHook::new(vec![Arc::new(Failing), Arc::new(StaticKey("sk-second"))]);
        let result = composite.resolve().await;
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().api_key.as_deref(),
            Some("sk-second"),
            "later success must override earlier error"
        );

        // [Err, Err] → first error preserved (root cause)
        let composite = CompositeAuthHook::new(vec![Arc::new(Failing), Arc::new(Failing)]);
        let result = composite.resolve().await;
        assert!(result.is_err());
    }
}
