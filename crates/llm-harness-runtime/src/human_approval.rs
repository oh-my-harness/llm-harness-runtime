use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{
    BeforeToolCallCtx, BeforeToolCallDecision, BeforeToolCallHook, ContentBlock, ToolResult,
};
use serde_json::Value;
use uuid::Uuid;

/// Lightweight human approval interface (business layer implements).
/// Only concerns asking the human — does not involve core hook protocol.
pub trait HumanApprover: Send + Sync {
    /// Ask a human whether to authorize. Returns `None` on timeout/no response.
    fn ask<'a>(&'a self, request: &'a ApprovalRequest) -> BoxFuture<'a, Option<ApprovalDecision>>;
}

/// Request for human approval — provides enough context to decide.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    /// Unique request identifier.
    pub request_id: String,
    /// Tool being requested.
    pub tool_name: String,
    /// Tool arguments.
    pub arguments: Value,
    /// Sufficient context for the human to make a judgment
    /// (current task description, last step result, etc.).
    pub context: String,
}

/// Human's decision.
///
/// Allow-once/allow-always UX state is maintained by the business layer
/// inside its `HumanApprover` implementation.
#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    Approve,
    Deny { reason: String },
}

/// Timeout policy when `HumanApprover::ask()` returns `None`.
#[derive(Debug, Clone)]
pub enum TimeoutPolicy {
    Approve,
    Deny { reason: String },
}

/// Adapts `HumanApprover` into a core `BeforeToolCallHook`.
///
/// Default timeout policy is `Deny` (fail-secure): registering a `HumanApprover`
/// signals an explicit need for human oversight; timeout means the supervisor
/// is unavailable, so the safe choice is to block rather than pass through.
pub struct HumanApprovalWrapper {
    approver: Arc<dyn HumanApprover>,
    on_timeout: TimeoutPolicy,
}

impl HumanApprovalWrapper {
    /// Create a new wrapper with the given approver and timeout policy.
    pub fn new(approver: Arc<dyn HumanApprover>, on_timeout: TimeoutPolicy) -> Self {
        Self {
            approver,
            on_timeout,
        }
    }
}

impl BeforeToolCallHook for HumanApprovalWrapper {
    fn on_call<'a>(&'a self, ctx: BeforeToolCallCtx<'a>) -> BoxFuture<'a, BeforeToolCallDecision> {
        Box::pin(async move {
            let context_text: String = ctx
                .assistant_message
                .content
                .iter()
                .filter_map(|block| match block {
                    llm_harness_types::ContentBlock::Text { text } => Some(text.as_str()),
                    llm_harness_types::ContentBlock::Thinking { thinking, .. } => {
                        Some(thinking.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");

            let context = if context_text.is_empty() {
                format!("Tool '{}' called at turn {}", ctx.tool_name, ctx.turn_index)
            } else {
                format!(
                    "Tool '{}' called at turn {}.\nAssistant context:\n{}",
                    ctx.tool_name, ctx.turn_index, context_text
                )
            };

            let request = ApprovalRequest {
                request_id: Uuid::new_v4().to_string(),
                tool_name: ctx.tool_name.to_owned(),
                arguments: ctx.args.clone(),
                context,
            };
            match self.approver.ask(&request).await {
                Some(ApprovalDecision::Approve) => BeforeToolCallDecision::Allow,
                Some(ApprovalDecision::Deny { reason }) => {
                    BeforeToolCallDecision::Deny(ToolResult {
                        content: vec![ContentBlock::Text {
                            text: format!(
                                "The operation was denied by the user.\nReason: {reason}\n\
                                 Please ask the user what they would like to do instead."
                            ),
                        }],
                        details: serde_json::json!({"reason": reason}),
                        terminate: true,
                    })
                }
                None => match &self.on_timeout {
                    TimeoutPolicy::Approve => BeforeToolCallDecision::Allow,
                    TimeoutPolicy::Deny { reason } => BeforeToolCallDecision::Deny(ToolResult {
                        content: vec![ContentBlock::Text {
                            text: format!(
                                "The operation was denied by the user (timeout).\nReason: {reason}\n\
                                 Please ask the user what they would like to do instead."
                            ),
                        }],
                        details: serde_json::json!({"reason": reason}),
                        terminate: true,
                    }),
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct TestApprover {
        decision: Option<ApprovalDecision>,
    }

    impl HumanApprover for TestApprover {
        fn ask<'a>(
            &'a self,
            _request: &'a ApprovalRequest,
        ) -> BoxFuture<'a, Option<ApprovalDecision>> {
            let d = self.decision.clone();
            Box::pin(async move { d })
        }
    }

    #[test]
    fn human_approver_is_object_safe() {
        fn _accept(_: &dyn HumanApprover) {}
    }

    #[test]
    fn human_approval_wrapper_implements_before_tool_call_hook() {
        let approver = Arc::new(TestApprover {
            decision: Some(ApprovalDecision::Approve),
        });
        let _wrapper: Arc<dyn BeforeToolCallHook> = Arc::new(HumanApprovalWrapper::new(
            approver,
            TimeoutPolicy::Deny {
                reason: "timeout".into(),
            },
        ));
    }
}
