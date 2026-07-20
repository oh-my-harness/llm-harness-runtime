use super::*;

use llm_harness_loop::ResponseFormat;
use llm_harness_types::{
    ContentBlock, PrepareNextTurnCtx, PrepareNextTurnHook, ToolError, ToolResult,
};

use crate::harness::prepare_next_turn::DefaultPrepareNextTurn;

/// Build a harness whose initial state carries the given `response_format`.
fn make_harness_with_response_format(rf: Option<ResponseFormat>) -> AgentHarness {
    let client = Arc::new(MockLlmClient::new(vec![]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.response_format = rf;
    futures::executor::block_on(AgentHarness::new_in_memory(
        client as Arc<dyn LlmClient>,
        env,
        opts,
    ))
}

/// Build a `PrepareNextTurnCtx` with a trivial text assistant message and
/// no tool results — enough to satisfy the hook signature.
fn make_ctx<'a>() -> PrepareNextTurnCtx<'a> {
    let assistant =
        llm_harness_loop::test_utils::test_assistant_message(vec![ContentBlock::Text {
            text: "x".into(),
        }]);
    let results: [(String, Result<ToolResult, ToolError>); 0] = [];
    PrepareNextTurnCtx {
        turn_index: 0,
        last_message: &assistant,
        last_tool_results: &results,
    }
}

#[tokio::test]
async fn default_prepare_next_turn_threads_state_response_format() {
    // State carries JsonObject → base directive must surface it.
    let h = make_harness_with_response_format(Some(ResponseFormat::JsonObject));

    let hook = DefaultPrepareNextTurn {
        inner: h.inner.clone(),
        user_hook: None,
    };

    let directive = hook.prepare(make_ctx()).await.unwrap();
    assert_eq!(
        directive.response_format,
        Some(Some(ResponseFormat::JsonObject)),
        "base directive must carry state's response_format as Some(Some(fmt))"
    );
}

#[tokio::test]
async fn default_prepare_next_turn_threads_none_response_format() {
    // State has no response_format → base directive carries Some(None),
    // meaning "leave it unset" (consistent with temperature handling).
    let h = make_harness_with_response_format(None);

    let hook = DefaultPrepareNextTurn {
        inner: h.inner.clone(),
        user_hook: None,
    };

    let directive = hook.prepare(make_ctx()).await.unwrap();
    assert_eq!(
        directive.response_format,
        Some(None),
        "base directive must carry Some(None) when state has no response_format"
    );
}

/// A user hook that returns a fixed `response_format` in its directive.
struct StaticRfHook(Option<Option<ResponseFormat>>);
impl PrepareNextTurnHook for StaticRfHook {
    fn prepare<'a>(
        &'a self,
        _ctx: PrepareNextTurnCtx<'a>,
    ) -> futures::future::BoxFuture<'a, Result<NextTurnDirective, AgentError>> {
        let rf = self.0;
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

#[tokio::test]
async fn default_prepare_next_turn_user_hook_overrides_response_format() {
    // State carries JsonObject, but the user hook sets JsonSchema → user wins.
    let h = make_harness_with_response_format(Some(ResponseFormat::JsonObject));

    let json_schema = ResponseFormat::JsonSchema {
        name: "out".into(),
        schema: serde_json::json!({"type": "object"}),
        strict: None,
    };
    let hook = DefaultPrepareNextTurn {
        inner: h.inner.clone(),
        user_hook: Some(Arc::new(StaticRfHook(Some(Some(json_schema.clone()))))),
    };

    let directive = hook.prepare(make_ctx()).await.unwrap();
    assert_eq!(
        directive.response_format,
        Some(Some(json_schema)),
        "user hook's response_format must override the state-derived base"
    );
}

#[tokio::test]
async fn default_prepare_next_turn_user_hook_none_preserves_base() {
    // User hook returns None for response_format → base (state) value is kept.
    let h = make_harness_with_response_format(Some(ResponseFormat::JsonObject));

    let hook = DefaultPrepareNextTurn {
        inner: h.inner.clone(),
        user_hook: Some(Arc::new(StaticRfHook(None))),
    };

    let directive = hook.prepare(make_ctx()).await.unwrap();
    assert_eq!(
        directive.response_format,
        Some(Some(ResponseFormat::JsonObject)),
        "user hook returning None must preserve the base response_format"
    );
}

#[tokio::test]
async fn default_prepare_next_turn_user_hook_clears_response_format() {
    // User hook returns Some(None) → explicit clear wins over base.
    let h = make_harness_with_response_format(Some(ResponseFormat::JsonObject));

    let hook = DefaultPrepareNextTurn {
        inner: h.inner.clone(),
        user_hook: Some(Arc::new(StaticRfHook(Some(None)))),
    };

    let directive = hook.prepare(make_ctx()).await.unwrap();
    assert_eq!(
        directive.response_format,
        Some(None),
        "user hook's Some(None) must clear the base response_format"
    );
}
