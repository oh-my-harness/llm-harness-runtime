//! `send_message` tool: send a direct message to another agent.

use std::sync::{Arc, LazyLock};

use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

use crate::controlled::{ControlledWorkerRun, DecisionWorkerRun, terminal_task_report};
use crate::{AgentId, GroupReplyContext, SocialContext, TaskMessageKind};

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
            "task_kind": {
                "type": "string",
                "enum": ["ack", "progress", "result", "blocked", "error"],
                "description": "Optional lifecycle meaning for a task-correlated message. ACK and PROGRESS are non-terminal; RESULT, BLOCKED, and ERROR are terminal and require a non-empty body."
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
    #[cfg(test)]
    pub(crate) fn call_inner(&self, input: &serde_json::Value) -> Result<String, ToolFailure> {
        self.call_inner_with_group(input, None)
    }

    #[allow(clippy::result_large_err)]
    pub(crate) fn call_inner_with_group(
        &self,
        input: &serde_json::Value,
        group: Option<&GroupReplyContext>,
    ) -> Result<String, ToolFailure> {
        let to_str = field_str(input, "to")?;
        let body = field_str(input, "body")?;
        let task_id = optional_str(input, "task_id");
        let task_kind = optional_task_kind(input)?;
        if task_kind.is_some_and(|kind| {
            matches!(
                kind,
                TaskMessageKind::Result | TaskMessageKind::Blocked | TaskMessageKind::Error
            )
        }) && terminal_task_report(task_kind, body).is_none()
        {
            return Err(ToolFailure::invalid_arguments(
                "terminal task messages require a non-empty body",
            ));
        }
        let broadcast_id = optional_str(input, "broadcast_id")
            .or_else(|| group.map(|group| group.broadcast_id.clone()));

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

        if to_str == crate::timeout_manager::reserved::OPERATOR && broadcast_id.is_some() {
            if let Some(responder) = group.and_then(|group| group.responder.as_deref())
                && self.agent_id.as_str() != responder
            {
                return Err(ToolFailure::invalid_arguments(format!(
                    "This public reply is reserved for '{responder}'. Use send_message to ask a peer directly instead."
                )));
            }
            return self.public_reply(body, broadcast_id);
        }
        self.ctx
            .send_direct_with_task_kind(
                &self.agent_id,
                &target_id,
                body,
                0,
                task_id,
                broadcast_id,
                task_kind,
            )
            .map_err(|error| {
                ToolFailure::new(
                    "inbox_journal_persistence_failed",
                    format!("failed to persist message: {error}"),
                )
            })?;
        Ok(format!("Message sent to {to_str}."))
    }

    #[allow(clippy::result_large_err)]
    fn public_reply(
        &self,
        body: &str,
        broadcast_id: Option<String>,
    ) -> Result<String, ToolFailure> {
        self.ctx
            .send_direct_with_task(
                &self.agent_id,
                &AgentId::new(crate::timeout_manager::reserved::OPERATOR),
                body,
                0,
                None,
                broadcast_id,
            )
            .map(|_| "sent".to_string())
            .map_err(|error| {
                ToolFailure::new(
                    "inbox_journal_persistence_failed",
                    format!("failed to persist public reply: {error}"),
                )
            })
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

#[allow(clippy::result_large_err)]
fn optional_task_kind(v: &serde_json::Value) -> Result<Option<TaskMessageKind>, ToolFailure> {
    let Some(value) = v.get("task_kind") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|_| ToolFailure::invalid_arguments("invalid task_kind"))
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
         If you are answering a public group message, use broadcast instead of \
         send_message so every member can see your reply.\n\n\
         When responding to a broadcast, include the broadcast_id from the \
         broadcast message so the sender can correlate your reply. Keep your \
         reply short (e.g. \"I can help, I have experience with X\") — do not \
         start working until formally delegated via delegate_task.\n\
         When responding to a delegation, include the task_id from the \
         delegation request so the sender can cancel any follow-up timer. For \
         controlled tasks, use task_kind=ack or progress for non-terminal \
         updates, and task_kind=result, blocked, or error for the one terminal \
         report. Terminal reports require a non-empty body.\n\n\
         Examples:\n\
         {\"to\": \"researcher-1\", \"body\": \"What's the status of the analysis?\"}\n\
         {\"to\": \"architect\", \"body\": \"I can help with this\", \"broadcast_id\": \"abc123\"}\n\
         {\"to\": \"coder-alpha\", \"body\": \"Tests passed\", \"task_id\": \"task-0\", \"task_kind\": \"result\"}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &SCHEMA
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let text =
                self.call_inner_with_group(&args, ctx.run.extension::<GroupReplyContext>())?;
            let terminal_delivered =
                if let Some(controlled) = ctx.run.extension::<ControlledWorkerRun>() {
                    controlled.record_delivery(
                        field_str(&args, "to")?,
                        optional_str(&args, "task_id").as_deref(),
                        optional_task_kind(&args)?,
                        field_str(&args, "body")?,
                    )
                } else if let Some(decision) = ctx.run.extension::<DecisionWorkerRun>() {
                    // A valid correlated ACTION is the Orchestrator's decision
                    // terminal: seal the run so no further provider turn or
                    // tool call (a second ACTION) can execute for this task.
                    decision.record_delivery(
                        field_str(&args, "to")?,
                        optional_str(&args, "task_id").as_deref(),
                        field_str(&args, "body")?,
                    )
                } else {
                    false
                };
            Ok(ToolResult::full(
                vec![DataBlock::text(text)],
                serde_json::Value::Null,
                // A durably delivered controlled terminal is the final tool
                // action for this run. This avoids another model turn while
                // the runner's terminal-seal path provides a hard fallback for
                // any harness work still unwinding after delivery.
                terminal_delivered,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GroupReplyContext;

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

    #[test]
    fn only_mentioned_agent_may_reply_publicly() {
        let ctx = SocialContext::new(10);
        let operator = AgentId::new(crate::timeout_manager::reserved::OPERATOR);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        let carol = AgentId::new("carol");
        ctx.register_agent(operator.clone(), "operator");
        ctx.register_agent(alice.clone(), "research");
        ctx.register_agent(bob.clone(), "coder");
        ctx.register_agent(carol.clone(), "reviewer");
        ctx.ensure_contact(&alice, &operator);
        ctx.ensure_contact(&bob, &operator);

        let group = GroupReplyContext {
            broadcast_id: "broadcast-1".to_string(),
            responder: Some("bob".to_string()),
        };
        let forbidden = SendMessageTool::new(alice.clone(), Arc::new(ctx.clone()));
        let error = forbidden
            .call_inner_with_group(
                &serde_json::json!({"to": "operator", "body": "I'll take this"}),
                Some(&group),
            )
            .unwrap_err();
        assert!(error.model_message.contains("reserved for 'bob'"));

        let allowed = SendMessageTool::new(bob.clone(), Arc::new(ctx.clone()));
        allowed
            .call_inner_with_group(
                &serde_json::json!({"to": "operator", "body": "on it"}),
                Some(&group),
            )
            .unwrap();
        assert!(ctx.has_messages(&operator));
        assert!(
            !ctx.has_messages(&carol),
            "a targeted public reply must not wake unrelated members"
        );
    }

    #[test]
    fn task_kind_is_validated_and_transported() {
        let temp = tempfile::tempdir().unwrap();
        let journal =
            Arc::new(crate::InboxJournal::open(temp.path().join("typed-message.sqlite3")).unwrap());
        let ctx = SocialContext::with_journal(10, Some(journal.clone()), None).unwrap();
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "implementation");
        ctx.register_agent(bob.clone(), "controller");
        ctx.ensure_contact(&alice, &bob);
        let tool = SendMessageTool::new(alice, Arc::new(ctx.clone()));

        tool.call_inner(&serde_json::json!({
            "to": "bob",
            "task_id": "task-1",
            "task_kind": "result",
            "body": "tests passed"
        }))
        .unwrap();

        let delivered = ctx.drain_inbox(&bob).remove(0);
        assert_eq!(delivered.message_type, crate::MessageType::Direct);
        assert_eq!(delivered.task_kind, Some(TaskMessageKind::Result));
        assert_eq!(delivered.body, "tests passed");

        let recovered = SocialContext::with_journal(10, Some(journal), None).unwrap();
        let persisted = recovered.drain_inbox(&bob).remove(0);
        assert_eq!(persisted.task_kind, Some(TaskMessageKind::Result));
        assert_eq!(persisted.task_id.as_deref(), Some("task-1"));

        let error = tool
            .call_inner(&serde_json::json!({
                "to": "bob",
                "task_id": "task-1",
                "task_kind": "complete",
                "body": "tests passed"
            }))
            .unwrap_err();
        assert!(error.model_message.contains("invalid task_kind"));
    }

    #[test]
    fn terminal_task_kind_requires_nonempty_body() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "implementation");
        ctx.register_agent(bob.clone(), "controller");
        ctx.ensure_contact(&alice, &bob);
        let tool = SendMessageTool::new(alice, Arc::new(ctx));

        let error = tool
            .call_inner(&serde_json::json!({
                "to": "bob",
                "task_id": "task-1",
                "task_kind": "blocked",
                "body": "  "
            }))
            .unwrap_err();

        assert!(error.model_message.contains("non-empty body"));
    }
}
