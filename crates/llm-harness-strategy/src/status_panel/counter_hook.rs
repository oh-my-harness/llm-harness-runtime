//! counter_hook.rs — ToolCallCounterHook (impl AfterToolCallHook).

use futures::future::BoxFuture;
use llm_harness_types::{AfterToolCallCtx, AfterToolCallDecision, AfterToolCallHook};

use crate::status_panel::collectors::ToolCallCounter;

pub struct ToolCallCounterHook {
    counter: ToolCallCounter,
}

impl ToolCallCounterHook {
    pub fn new(counter: ToolCallCounter) -> Self {
        Self { counter }
    }

    pub fn counter(&self) -> &ToolCallCounter {
        &self.counter
    }
}

impl AfterToolCallHook for ToolCallCounterHook {
    fn on_complete<'a>(
        &'a self,
        ctx: AfterToolCallCtx<'a>,
    ) -> BoxFuture<'a, AfterToolCallDecision> {
        self.counter.increment(ctx.tool_name);
        Box::pin(async move { AfterToolCallDecision::Passthrough })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use llm_harness_types::{
        AfterToolCallCtx, AssistantMessage, AssistantMessageKind, RunContext, RunRequest,
        ToolResult,
    };
    use serde_json::json;

    fn make_ctx<'a>(tool_name: &'a str) -> AfterToolCallCtx<'a> {
        let assistant = Box::leak(Box::new(AssistantMessage {
            kind: AssistantMessageKind::Progress,
            message_id: "m1".into(),
            turn_id: "t1".into(),
            content: vec![],
            stop_reason: None,
            timestamp: Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        }));
        let run = Box::leak(Box::new(RunContext::new(RunRequest::default())));
        let result = Box::leak(Box::new(Ok(ToolResult::full(
            vec![llm_harness_types::DataBlock::text("ok")],
            json!(null),
            false,
        ))));
        let args = Box::leak(Box::new(json!({})));
        AfterToolCallCtx {
            run,
            assistant_message: assistant,
            tool_use_id: "c1",
            tool_name,
            args,
            result,
            turn_index: 0,
        }
    }

    #[tokio::test]
    async fn increments_counter_on_complete() {
        let counter = ToolCallCounter::new();
        let hook = ToolCallCounterHook::new(counter.clone());
        let ctx = make_ctx("bash");
        let decision = hook.on_complete(ctx).await;
        assert!(matches!(decision, AfterToolCallDecision::Passthrough));
        let snap = counter.snapshot();
        assert_eq!(snap, vec![("bash".to_string(), 1)]);
    }

    #[tokio::test]
    async fn multiple_calls_accumulate() {
        let counter = ToolCallCounter::new();
        let hook = ToolCallCounterHook::new(counter.clone());
        hook.on_complete(make_ctx("bash")).await;
        hook.on_complete(make_ctx("bash")).await;
        hook.on_complete(make_ctx("read")).await;
        let snap = counter.snapshot();
        assert_eq!(snap[0], ("bash".to_string(), 2));
        assert_eq!(snap[1], ("read".to_string(), 1));
    }
}
