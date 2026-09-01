use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{
    AfterProviderResponseCtx, AfterProviderResponseHook, AfterRunHook, AfterToolCallCtx,
    AfterToolCallDecision, AfterToolCallHook, AfterTurnCtx, AfterTurnHook, AgentContext,
    AgentError, BeforeCompactCtx, BeforeCompactDecision, BeforeCompactHook,
    BeforeProviderRequestCtx, BeforeProviderRequestHook, BeforeRunCtx, BeforeRunHook,
    BeforeRunResult, BeforeToolCallCtx, BeforeToolCallDecision, BeforeToolCallHook, BeforeTurnCtx,
    BeforeTurnHook, ErrorDirective, FinalAnswerValidationCtx, FinalAnswerValidationError,
    FinalAnswerValidator, PrepareNextTurnCtx, PrepareNextTurnHook, ProviderErrorCtx,
    ProviderErrorHook, ShouldStopCtx, ShouldStopHook, TransformContextCtx, TransformContextHook,
};

/// Chains final-answer validators in registration order.
///
/// Validation short-circuits on the first rejection, so no later validator can
/// accidentally turn a rejected answer back into an accepted one.
pub struct CompositeFinalAnswerValidator {
    validators: Vec<Arc<dyn FinalAnswerValidator>>,
}

impl CompositeFinalAnswerValidator {
    /// Creates a validator chain.
    pub fn new(validators: Vec<Arc<dyn FinalAnswerValidator>>) -> Self {
        Self { validators }
    }
}

impl FinalAnswerValidator for CompositeFinalAnswerValidator {
    fn validate<'a>(
        &'a self,
        ctx: FinalAnswerValidationCtx<'a>,
    ) -> BoxFuture<'a, Result<(), FinalAnswerValidationError>> {
        Box::pin(async move {
            for validator in &self.validators {
                validator
                    .validate(FinalAnswerValidationCtx {
                        run: ctx.run,
                        candidate: ctx.candidate,
                        turn_index: ctx.turn_index,
                    })
                    .await?;
            }
            Ok(())
        })
    }
}

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
    fn after_response<'a>(&'a self, ctx: AfterProviderResponseCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.hooks {
                hook.after_response(AfterProviderResponseCtx {
                    run: ctx.run,
                    turn_index: ctx.turn_index,
                    info: ctx.info,
                })
                .await;
            }
        })
    }
}

/// Chains multiple `AfterToolCallHook` implementations (chained semantics).
///
/// Each hook sees the result from the previous hook's replacement (or the
/// original result if no prior hook replaced). When a hook returns `Replace`,
/// its output becomes the input to the next hook. `Passthrough` leaves the
/// current result unchanged. This allows multiple hooks to compose their
/// modifications — e.g. an injection filter and a source-tag wrapper can
/// both apply to the same tool result.
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
            // Start from the original result. Each hook that returns Replace
            // feeds its output into the next hook, so modifications compose.
            let mut current_result = ctx.result.clone();
            let mut replaced = false;

            for hook in &self.hooks {
                let hook_ctx = AfterToolCallCtx {
                    run: ctx.run,
                    assistant_message: ctx.assistant_message,
                    tool_use_id: ctx.tool_use_id,
                    tool_name: ctx.tool_name,
                    args: ctx.args,
                    result: &current_result,
                    duration: ctx.duration,
                    turn_index: ctx.turn_index,
                };

                let decision = hook.on_complete(hook_ctx).await;

                if let AfterToolCallDecision::Replace(new_result) = decision {
                    current_result = new_result;
                    replaced = true;
                }
            }

            if replaced {
                AfterToolCallDecision::Replace(current_result)
            } else {
                AfterToolCallDecision::Passthrough
            }
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
                    run: ctx.run,
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
                    run: ctx.run,
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
    fn before_request<'a>(&'a self, ctx: BeforeProviderRequestCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.hooks {
                hook.before_request(BeforeProviderRequestCtx {
                    run: ctx.run,
                    turn_index: ctx.turn_index,
                    options: &mut *ctx.options,
                })
                .await;
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
                        run: ctx.run,
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
                    run: ctx.run,
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
        ctx: TransformContextCtx<'a>,
    ) -> BoxFuture<'a, Result<AgentContext, AgentError>> {
        Box::pin(async move {
            let mut context = ctx.context;
            for hook in &self.hooks {
                context = hook
                    .transform(TransformContextCtx {
                        run: ctx.run,
                        turn_index: ctx.turn_index,
                        abort: ctx.abort.clone(),
                        context,
                    })
                    .await?;
            }
            Ok(context)
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
                response_format: None,
            };

            for hook in &self.hooks {
                let result = hook
                    .prepare(PrepareNextTurnCtx {
                        run: ctx.run,
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
                if result.response_format.is_some() {
                    directive.response_format = result.response_format;
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
/// (`AsyncSpawnHook`) that must not be skipped just because
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
                    run: ctx.run,
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
                    entry_ids: ctx.entry_ids,
                    context_window: ctx.context_window,
                    reserve_tokens: ctx.reserve_tokens,
                    keep_recent_tokens: ctx.keep_recent_tokens,
                    current_query: ctx.current_query,
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

/// Chains multiple `AfterRunHook` implementations (all-execute semantics).
///
/// All hooks execute in registration order (observing only, no return value).
pub struct CompositeAfterRunHook {
    hooks: Vec<Arc<dyn AfterRunHook>>,
}

impl CompositeAfterRunHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn AfterRunHook>>) -> Self {
        Self { hooks }
    }
}

impl AfterRunHook for CompositeAfterRunHook {
    fn after_run(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            for hook in &self.hooks {
                hook.after_run().await;
            }
        })
    }
}

/// Chains multiple `ProviderErrorHook` implementations (first-`Retry` wins).
///
/// Hooks execute in registration order; the first hook returning `Retry`
/// short-circuits the chain and the loop retries the turn. Every hook sees
/// the same `&mut` views of the context, so a `Surface` after an earlier
/// `Retry`-mutating hook would surface an already-repaired context — in
/// practice callers register repairing hooks only.
pub struct CompositeProviderErrorHook {
    hooks: Vec<Arc<dyn ProviderErrorHook>>,
}

impl CompositeProviderErrorHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn ProviderErrorHook>>) -> Self {
        Self { hooks }
    }
}

impl ProviderErrorHook for CompositeProviderErrorHook {
    fn on_provider_error<'a>(&'a self, ctx: ProviderErrorCtx<'a>) -> BoxFuture<'a, ErrorDirective> {
        Box::pin(async move {
            for hook in &self.hooks {
                let directive = hook
                    .on_provider_error(ProviderErrorCtx {
                        run: ctx.run,
                        turn_index: ctx.turn_index,
                        error: ctx.error,
                        // Each hook gets the same disjoint views; reborrowed
                        // sequentially through the original `&mut`s.
                        context: ctx.context,
                        new_messages: ctx.new_messages,
                    })
                    .await;
                if matches!(directive, ErrorDirective::Retry) {
                    return directive;
                }
            }
            ErrorDirective::Surface
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
    fn composite_provider_error_hook_is_object_safe() {
        fn _accept(_: &dyn ProviderErrorHook) {}
        let composite = CompositeProviderErrorHook::new(vec![]);
        _accept(&composite);
    }
    #[test]
    fn composite_after_tool_call_hook_is_object_safe() {
        fn _accept(_: &dyn AfterToolCallHook) {}
        let composite = CompositeAfterToolCallHook::new(vec![]);
        _accept(&composite);
    }

    #[tokio::test]
    async fn composite_after_tool_call_chains_replacements() {
        // Regression: two Replace hooks must both have their modifications
        // survive. Previously the composite used last-Replace-wins, which
        // silently discarded the first hook's output.
        use llm_harness_types::{DataBlock, RunContext, RunRequest, ToolResult};

        /// Prefixes text blocks with "A:".
        struct PrefixHook(&'static str);
        impl AfterToolCallHook for PrefixHook {
            fn on_complete<'a>(
                &'a self,
                ctx: AfterToolCallCtx<'a>,
            ) -> BoxFuture<'a, AfterToolCallDecision> {
                let prefix = self.0;
                Box::pin(async move {
                    let Ok(result) = &ctx.result else {
                        return AfterToolCallDecision::Passthrough;
                    };
                    let new_content: Vec<DataBlock> = result
                        .model_content
                        .iter()
                        .map(|b| match b {
                            DataBlock::Text { text, mime_type } => DataBlock::Text {
                                text: format!("{prefix}: {text}"),
                                mime_type: mime_type.clone(),
                            },
                            o => o.clone(),
                        })
                        .collect();
                    AfterToolCallDecision::Replace(Ok(ToolResult {
                        model_content: new_content,
                        details: result.details.clone(),
                        extensions: result.extensions.clone(),
                        terminate: result.terminate,
                        session_projection: result.session_projection.clone(),
                    }))
                })
            }
        }

        let run = RunContext::new(RunRequest::from_text("test"));
        let assistant = crate::test_utils::test_assistant_message(vec![]);
        let result = Ok(ToolResult::full(
            vec![DataBlock::text("content")],
            serde_json::json!({}),
            false,
        ));
        let ctx = AfterToolCallCtx {
            run: &run,
            assistant_message: &assistant,
            tool_use_id: "tu-1",
            tool_name: "web_search",
            args: &serde_json::Value::Null,
            result: &result,
            duration: std::time::Duration::from_millis(42),
            turn_index: 0,
        };

        let composite = CompositeAfterToolCallHook::new(vec![
            Arc::new(PrefixHook("A")),
            Arc::new(PrefixHook("B")),
        ]);
        let decision = composite.on_complete(ctx).await;

        let AfterToolCallDecision::Replace(Ok(replaced)) = decision else {
            panic!("expected Replace");
        };
        let text = match &replaced.model_content[0] {
            DataBlock::Text { text, .. } => text.as_str(),
            _ => panic!("expected Text"),
        };
        // Both hooks applied: B saw A's output, so order is "B: A: content".
        assert_eq!(text, "B: A: content");
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
    #[tokio::test]
    async fn composite_prepare_next_turn_merges_response_format_last_wins() {
        use crate::ResponseFormat;
        use llm_harness_types::{
            AgentError, ContentBlock, NextTurnDirective, ToolFailure, ToolResult,
        };

        struct StaticHook(Option<Option<ResponseFormat>>);
        impl PrepareNextTurnHook for StaticHook {
            fn prepare<'a>(
                &'a self,
                _ctx: PrepareNextTurnCtx<'a>,
            ) -> BoxFuture<'a, Result<NextTurnDirective, AgentError>> {
                let rf = self.0.clone();
                Box::pin(async move {
                    Ok(NextTurnDirective {
                        context: None,
                        model: None,
                        thinking_level: None,
                        temperature: None,
                        tools: None,
                        active_tools: None,
                        response_format: rf,
                    })
                })
            }
        }

        let assistant = crate::test_utils::test_assistant_message(vec![ContentBlock::Text {
            text: "x".into(),
        }]);
        let results: [(String, Result<ToolResult, ToolFailure>); 0] = [];
        let run = llm_harness_types::RunContext::new(llm_harness_types::RunRequest::default());
        let mk_ctx = || PrepareNextTurnCtx {
            run: &run,
            turn_index: 0,
            last_message: &assistant,
            last_tool_results: &results,
        };

        // None hook leaves response_format unset; second hook sets it.
        let composite = CompositePrepareNextTurnHook::new(vec![
            Arc::new(StaticHook(None)),
            Arc::new(StaticHook(Some(Some(ResponseFormat::JsonObject)))),
        ]);
        let d = composite.prepare(mk_ctx()).await.unwrap();
        assert_eq!(
            d.response_format,
            Some(Some(ResponseFormat::JsonObject)),
            "later hook's response_format must win"
        );

        // Later non-None overrides earlier non-None.
        let composite = CompositePrepareNextTurnHook::new(vec![
            Arc::new(StaticHook(Some(Some(ResponseFormat::JsonObject)))),
            Arc::new(StaticHook(Some(None))),
        ]);
        let d = composite.prepare(mk_ctx()).await.unwrap();
        assert_eq!(
            d.response_format,
            Some(None),
            "later Some(None) must override earlier Some(Some(_))"
        );

        // All None → merged stays None.
        let composite = CompositePrepareNextTurnHook::new(vec![
            Arc::new(StaticHook(None)),
            Arc::new(StaticHook(None)),
        ]);
        let d = composite.prepare(mk_ctx()).await.unwrap();
        assert_eq!(
            d.response_format, None,
            "all-None hooks must leave response_format as None"
        );
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
        let run = llm_harness_types::RunContext::new(llm_harness_types::RunRequest::default());
        let ctx = ShouldStopCtx {
            run: &run,
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
}
