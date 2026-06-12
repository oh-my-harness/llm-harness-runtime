use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{
    AfterProviderResponseHook, AfterToolCallCtx, AfterToolCallDecision, AfterToolCallHook,
    AfterTurnCtx, AfterTurnHook, BeforeProviderRequestHook, BeforeToolCallCtx,
    BeforeToolCallDecision, BeforeToolCallHook, ProviderResponseInfo, StreamOptions,
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

/// Chains multiple `AfterToolCallHook` implementations (last-non-passthrough semantics).
///
/// Hooks execute in order; each sees the original ctx. The last `Patch` decision wins.
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
            let mut decision = AfterToolCallDecision::Passthrough;
            for hook in &self.hooks {
                // Reconstruct ctx for each hook (AfterToolCallCtx is not Clone).
                let hook_ctx = AfterToolCallCtx {
                    assistant_message: ctx.assistant_message,
                    tool_use_id: ctx.tool_use_id,
                    tool_name: ctx.tool_name,
                    args: ctx.args,
                    result: ctx.result,
                    turn_index: ctx.turn_index,
                };
                let d = hook.on_complete(hook_ctx).await;
                if !matches!(d, AfterToolCallDecision::Passthrough) {
                    decision = d;
                }
            }
            decision
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
}
