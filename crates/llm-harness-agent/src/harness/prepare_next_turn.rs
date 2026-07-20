// prepare_next_turn.rs
use std::sync::{Arc, Mutex};

use llm_harness_types::{AgentError, NextTurnDirective, PrepareNextTurnCtx, PrepareNextTurnHook};

use super::state::HarnessInner;

// ── Wrapper for default PrepareNextTurnHook ───────────────────────────────────

/// Default wrapper around `PrepareNextTurnHook` that reads `HarnessInner` to
/// propagate `active_tools`, `model`, and `thinking_level` to the next turn.
pub(super) struct DefaultPrepareNextTurn {
    pub(super) inner: Arc<Mutex<HarnessInner>>,
    pub(super) user_hook: Option<Arc<dyn PrepareNextTurnHook>>,
}

impl PrepareNextTurnHook for DefaultPrepareNextTurn {
    fn prepare<'a>(
        &'a self,
        ctx: PrepareNextTurnCtx<'a>,
    ) -> futures::future::BoxFuture<'a, Result<NextTurnDirective, AgentError>> {
        Box::pin(async move {
            // Short lock: read current config, then release immediately.
            let (model, thinking_level, temperature, response_format, tools, active_tools) = {
                let g = self.inner.lock().unwrap();
                let st = &g.state;
                (
                    st.model.clone(),
                    st.thinking_level,
                    st.temperature,
                    st.response_format.clone(),
                    st.tools.clone(),
                    st.active_tools.clone(),
                )
            };

            let mut directive = NextTurnDirective {
                context: None,
                model: Some(model),
                thinking_level: Some(thinking_level),
                temperature: Some(temperature),
                tools: Some(tools),
                active_tools,
                response_format: Some(response_format),
            };

            // Chain user-provided hook: its non-None fields win.
            if let Some(ref user_hook) = self.user_hook {
                let user = user_hook.prepare(ctx).await?;
                if user.context.is_some() {
                    directive.context = user.context;
                }
                if user.model.is_some() {
                    directive.model = user.model;
                }
                if user.thinking_level.is_some() {
                    directive.thinking_level = user.thinking_level;
                }
                if user.temperature.is_some() {
                    directive.temperature = user.temperature;
                }
                if user.tools.is_some() {
                    directive.tools = user.tools;
                }
                if user.active_tools.is_some() {
                    directive.active_tools = user.active_tools;
                }
                if user.response_format.is_some() {
                    directive.response_format = user.response_format;
                }
            }

            Ok(directive)
        })
    }
}
