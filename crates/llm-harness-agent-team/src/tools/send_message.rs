//! `send_message` tool: send a direct message to another agent.

use std::sync::{Arc, LazyLock};

use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

use crate::{AgentId, SocialContext};

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "to": {
                "type": "string",
                "description": "The agent ID of the recipient. e.g. \"researcher-1\", \"coder-alpha\""
            },
            "body": {
                "type": "string",
                "description": "The message body to send. e.g. \"Can you help me analyze this data?\""
            },
            "task_id": {
                "type": "string",
                "description": "Optional. When replying to a delegated task, echo the task_id from the delegation request so the sender can correlate and cancel any pending follow-up timer."
            },
            "broadcast_id": {
                "type": "string",
                "description": "Optional. When responding to a broadcast, echo the broadcast_id shown in the broadcast message so the sender can correlate your reply with the original broadcast."
            }
        },
        "required": ["to", "body"],
        "additionalProperties": false
    })
});

/// Tool that sends a direct message to a peer agent.
pub struct SendMessageTool {
    agent_id: AgentId,
    ctx: Arc<SocialContext>,
}

impl SendMessageTool {
    /// Send on behalf of `agent_id` through the shared social context.
    pub fn new(agent_id: AgentId, ctx: Arc<SocialContext>) -> Self {
        Self { agent_id, ctx }
    }

    // ToolFailure is ~144 bytes; the harness Tool trait boxes the future so
    // its own signatures never trip this lint. Mirrors failure_breaker.rs.
    #[allow(clippy::result_large_err)]
    pub(crate) fn call_inner(&self, input: &serde_json::Value) -> Result<String, ToolFailure> {
        let to_str = field_str(input, "to")?;
        let body = field_str(input, "body")?;
        let task_id = optional_str(input, "task_id");
        let broadcast_id = optional_str(input, "broadcast_id");

        let target_id = AgentId::new(to_str);

        // Reject unregistered targets — matching delegate_task — so messages
        // aren't delivered to an inbox nobody drains.
        if self.ctx.get_agent_info(&target_id).is_none() {
            return Err(ToolFailure::invalid_arguments(format!(
                "Error: Agent '{to_str}' is not a registered team member and not in your contact list. Use delegate_task to reach outside your contacts."
            )));
        }
        // The target must be a known contact.
        if !self.ctx.has_contact(&self.agent_id, &target_id) {
            return Err(ToolFailure::invalid_arguments(format!(
                "Agent '{to_str}' is not in your contact list."
            )));
        }

        self.ctx
            .send_direct_with_task(&self.agent_id, &target_id, body, 0, task_id, broadcast_id);
        Ok(format!("Message sent to {to_str}."))
    }
}

/// Extract a required string field; `Err` when absent or not a string.
#[allow(clippy::result_large_err)]
fn field_str<'a>(v: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolFailure> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ToolFailure::invalid_arguments(format!("missing string field '{key}'")))
}

/// Extract an optional string field; `None` when absent or not a string.
fn optional_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn label(&self) -> &str {
        "Send message"
    }

    fn description(&self) -> &str {
        "Send a direct message to another agent in your contact list. \
         The recipient will find it in their inbox.\n\n\
         Use when: asking a specific peer a question; sharing information with \
         one agent; responding to a delegation; volunteering to help with a \
         broadcast.\n\
         Don't use for: sending to multiple agents (use broadcast); delegating \
         a task with tracking (use delegate_task); reaching agents not in your \
         contact list (use search_contacts first).\n\n\
         When responding to a broadcast, include the broadcast_id from the \
         broadcast message so the sender can correlate your reply. Keep your \
         reply short (e.g. \"I can help, I have experience with X\") — do not \
         start working until formally delegated via delegate_task.\n\
         When responding to a delegation, include the task_id from the \
         delegation request so the sender can cancel any follow-up timer.\n\n\
         Examples:\n\
         {\"to\": \"researcher-1\", \"body\": \"What's the status of the analysis?\"}\n\
         {\"to\": \"architect\", \"body\": \"I can help with this\", \"broadcast_id\": \"abc123\"}\n\
         {\"to\": \"coder-alpha\", \"body\": \"Done, here are the results\", \"task_id\": \"task-0\"}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &SCHEMA
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let text = self.call_inner(&args)?;
            Ok(ToolResult::full(
                vec![DataBlock::text(text)],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unregistered_target_error_mentions_delegate_task() {
        // T14 (C2): the unregistered-peer rejection must not lie about the
        // reason and must point the agent at the escape hatch — delegate_task
        // reaches peers outside the contact list.
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        let tool = SendMessageTool::new(alice.clone(), Arc::new(ctx.clone()));
        let err = tool
            .call_inner(&serde_json::json!({"to": "ghost", "body": "hello"}))
            .unwrap_err();

        assert!(
            err.model_message.contains("not a registered team member"),
            "unexpected message: {}",
            err.model_message
        );
        assert!(
            err.model_message.contains("delegate_task"),
            "error must mention the delegate_task escape hatch: {}",
            err.model_message
        );
        // No message may have been delivered to the unknown peer.
        assert!(!ctx.has_messages(&AgentId::new("ghost")));
    }
}
