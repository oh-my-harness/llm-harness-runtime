//! RuleBasedApprovalHook —— 实现 BeforeToolCallHook，用 RuleChain 做工具调用审批。

use futures::future::BoxFuture;
use llm_harness_types::{
    BeforeToolCallCtx, BeforeToolCallDecision, BeforeToolCallHook, ContentBlock, ToolResult,
};

use crate::rules::chain::RuleChain;
use crate::rules::rule::Decision;

/// 基于 `RuleChain` 的工具调用审批 hook。
///
/// fail-secure：`RuleChain` 全不命中时默认 Deny（由 RuleChain.fallback 决定）。
pub struct RuleBasedApprovalHook {
    chain: RuleChain,
}

impl RuleBasedApprovalHook {
    pub fn new(chain: RuleChain) -> Self {
        Self { chain }
    }
}

impl BeforeToolCallHook for RuleBasedApprovalHook {
    fn on_call<'a>(&'a self, ctx: BeforeToolCallCtx<'a>) -> BoxFuture<'a, BeforeToolCallDecision> {
        let decision = self.chain.evaluate(ctx.tool_name, ctx.args);
        Box::pin(async move {
            match decision {
                Decision::Allow => BeforeToolCallDecision::Allow,
                Decision::Deny => BeforeToolCallDecision::Deny(ToolResult {
                    content: vec![ContentBlock::Text {
                        text: "rule denied".into(),
                    }],
                    details: serde_json::Value::Null,
                    terminate: false,
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::predicates::Contains;
    use crate::rules::rule::Rule;
    use llm_harness_types::AssistantMessage;

    /// 构造一个最小的 ctx。`msg` 必须比 ctx 活得久——调用方持有 msg。
    fn make_ctx<'a>(msg: &'a AssistantMessage, tool_name: &'a str) -> BeforeToolCallCtx<'a> {
        BeforeToolCallCtx {
            assistant_message: msg,
            tool_use_id: "tu_1",
            tool_name,
            args: &serde_json::Value::Null,
            turn_index: 0,
        }
    }

    fn empty_message() -> AssistantMessage {
        llm_harness_loop::test_utils::test_assistant_message(vec![])
    }

    #[tokio::test]
    async fn allow_when_rule_matches() {
        let chain = RuleChain::builder()
            .rule(Rule {
                tool_name: "measure".into(),
                predicate: Box::new(Contains::new(vec!["measure".into()])),
                on_match: Decision::Allow,
            })
            .fallback(Decision::Deny)
            .build();
        let hook = RuleBasedApprovalHook::new(chain);
        let msg = empty_message();
        let ctx = make_ctx(&msg, "measure");
        let decision = hook.on_call(ctx).await;
        assert!(matches!(decision, BeforeToolCallDecision::Allow));
    }

    #[tokio::test]
    async fn deny_when_no_match() {
        let chain = RuleChain::builder().fallback(Decision::Deny).build();
        let hook = RuleBasedApprovalHook::new(chain);
        let msg = empty_message();
        let ctx = make_ctx(&msg, "dangerous_tool");
        let decision = hook.on_call(ctx).await;
        assert!(matches!(decision, BeforeToolCallDecision::Deny(_)));
    }
}
