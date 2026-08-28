//! `delegate_task` tool: delegate a task to another agent.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use crate::inbox::{InboxMessage, MessageType};
use crate::timeout_manager::TimeoutManager;
use crate::{AgentId, SocialContext};
use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "to": {
                "type": "string",
                "description": "The agent ID of the delegate. e.g. \"researcher-1\", \"coder-alpha\""
            },
            "task": {
                "type": "string",
                "description": "The task description to delegate. e.g. \"Analyze the query performance of the users table\""
            },
            "timeout_secs": {
                "type": "integer",
                "description": "How long (in seconds) to wait for the delegate's reply before being nudged to follow up. If omitted, uses the system default. Set a longer timeout for complex tasks, shorter for simple ones.",
                "minimum": 1
            }
        },
        "required": ["to", "task"],
        "additionalProperties": false
    })
});

/// Tool that delegates a task to a peer agent via their inbox.
///
/// After delegating, schedules a `Timeout` message (keyed by the task_id)
/// so the originator is nudged if the delegate doesn't reply in time. The
/// timeout is automatically cancelled when a reply carrying the same
/// task_id is drained by the runner.
pub struct DelegateTaskTool {
    agent_id: AgentId,
    ctx: Arc<SocialContext>,
    task_counter: AtomicU64,
    timeout: Duration,
    /// Explicitly configured operator id; `None` disables the operator check.
    operator_id: Option<AgentId>,
}

impl DelegateTaskTool {
    /// Create the tool for `agent_id`; delegation requests are pushed through
    /// the shared social context and replies awaited up to `timeout`.
    pub fn new(agent_id: AgentId, ctx: Arc<SocialContext>, timeout: Duration) -> Self {
        Self {
            agent_id,
            ctx,
            task_counter: AtomicU64::new(0),
            timeout,
            operator_id: None,
        }
    }

    /// Set the operator id; delegating to the operator is then rejected.
    pub fn with_operator(mut self, id: AgentId) -> Self {
        self.operator_id = Some(id);
        self
    }

    // ToolFailure is ~144 bytes; the harness Tool trait boxes the future so
    // its own signatures never trip this lint. Mirrors failure_breaker.rs.
    #[allow(clippy::result_large_err)]
    pub(crate) fn call_inner(&self, input: &serde_json::Value) -> Result<String, ToolFailure> {
        let to_str = input
            .get("to")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolFailure::invalid_arguments("missing string field 'to'"))?;
        let task = input
            .get("task")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolFailure::invalid_arguments("missing string field 'task'"))?;

        // Reject delegation to unregistered agents — prevents ghost contacts
        // and messages delivered to an inbox nobody drains.
        if self.ctx.get_agent_info(&AgentId::new(to_str)).is_none() {
            return Err(ToolFailure::invalid_arguments(format!(
                "Agent '{to_str}' is not a registered team member."
            )));
        }
        let target_id = AgentId::new(to_str);

        // Reject delegation to the operator: it is not a team member with a
        // runner — its inbox is drained by the human, and a task sent there
        // would never be picked up by the team. Only when an operator id was
        // explicitly configured via with_operator.
        if self.operator_id.as_ref().is_some_and(|op| *op == target_id) {
            return Err(ToolFailure::invalid_arguments(format!(
                "Cannot delegate to the operator '{to_str}': tasks must go to a \
                 team member. Delegate to a registered peer instead."
            )));
        }

        // Reject self-delegation: it pushes a message into your own inbox
        // and schedules a follow-up timer you answer yourself — a pointless
        // self-message loop.
        if target_id == self.agent_id {
            return Err(ToolFailure::invalid_arguments(format!(
                "Cannot delegate to yourself ('{to_str}'): do the task directly \
                 instead of delegating it back to yourself."
            )));
        }

        let task_id = self.task_counter.fetch_add(1, Ordering::Relaxed);
        let task_id_str = format!("task-{}-{task_id}", self.agent_id.as_str());

        // Ensure the delegate is a contact and register communication.
        self.ctx.touch_contact(
            &self.agent_id,
            &target_id,
            crate::relation::RelationEvent::Communication,
        );

        let message = InboxMessage {
            from: self.agent_id.clone(),
            to: target_id.clone(),
            body: task.to_string(),
            message_type: MessageType::DelegationRequest,
            priority: crate::inbox::priority::DELEGATION,
            task_id: Some(task_id_str.clone()),
            broadcast_id: None,
        };
        self.ctx.push_message(message);
        // Schedule a cancellable timeout keyed by the task_id. When the
        // delegate replies (carrying the same task_id), the runner cancels
        // this timeout. If it fires, the originator is nudged to follow up.
        // The agent can override the default timeout via timeout_secs.
        let timeout = input
            .get("timeout_secs")
            .and_then(super::parse_timeout_secs)
            .unwrap_or(self.timeout);
        let timer = TimeoutManager::new((*self.ctx).clone());
        timer.set_timeout(
            self.agent_id.clone(),
            format!(
                "Delegated task '{task_id_str}' to {to_str} has not received a reply \
                 within {} seconds. Follow up: send a message to {to_str} to ask for a \
                 status update, re-delegate, do it yourself, or proceed.",
                timeout.as_secs()
            ),
            timeout,
            Some(task_id_str.clone()),
            Some(target_id.clone()),
        );
        let timeout_note = format!(" A {}s follow-up timer is set.", timeout.as_secs());

        Ok(format!(
            "Task '{task_id_str}' delegated to {to_str}.{timeout_note}"
        ))
    }
}

impl Tool for DelegateTaskTool {
    fn name(&self) -> &str {
        "delegate_task"
    }

    fn label(&self) -> &str {
        "Delegate task"
    }

    fn description(&self) -> &str {
        "Delegate a task to another agent. The task is delivered to their \
         inbox as a delegation request. Returns the assigned task ID.\n\n\
         Use when: you have a sub-task that fits a peer's expertise; distributing work; requesting a specific agent to do something with tracking.\n\
         Don't use for: casual messaging without task tracking (use send_message); reaching all agents (use broadcast); finding the right agent (use search_contacts first).\n\
         Returns: The assigned task ID and confirmation of delivery to the delegate's inbox.\n\n\
         The delegate should reply via send_message with the task_id when done. \
         If they don't reply within the timeout, you will be nudged to follow up. \
         You can specify timeout_secs to adjust the wait based on task complexity.\n\n\
         Examples:\n\
         {\"to\": \"researcher-1\", \"task\": \"Find all references to deprecated APIs\", \"timeout_secs\": 600}\n\
         {\"to\": \"coder-alpha\", \"task\": \"Implement input validation for the login form\"}"
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
    use crate::social_context::TimeoutCancel;
    use crate::timeout_manager::reserved;

    #[tokio::test]
    async fn delegate_with_timeout_sets_cancellable_timer() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        let tool = DelegateTaskTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(30),
        );

        let result = tool
            .call_inner(&serde_json::json!({"to": "bob", "task": "review PR"}))
            .unwrap();

        // Tool reports the timer.
        assert!(result.contains("task-alice-0"));
        assert!(result.contains("30s follow-up timer"));

        // The timeout is registered under (alice, "task-alice-0"),
        // targeting bob.
        assert_eq!(
            ctx.cancel_timeout(&alice, "task-alice-0"),
            TimeoutCancel::Cancelled {
                peer: Some(bob.clone())
            }
        );
    }

    #[tokio::test]
    async fn delegate_agent_specified_timeout_overrides_default() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        let tool = DelegateTaskTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(300),
        );
        let result = tool
            .call_inner(&serde_json::json!({"to": "bob", "task": "review PR", "timeout_secs": 60}))
            .unwrap();

        // Agent-specified timeout (60s) overrides the default (300s).
        assert!(result.contains("60s follow-up timer"));
        assert!(!result.contains("300s"));
    }

    #[tokio::test]
    async fn delegate_reply_cancels_timeout() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);
        ctx.ensure_contact(&bob, &alice);

        let tool = DelegateTaskTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(30),
        );
        tool.call_inner(&serde_json::json!({"to": "bob", "task": "review PR"}))
            .unwrap();

        // Bob replies via send_message with the task_id.
        let send =
            crate::tools::send_message::SendMessageTool::new(bob.clone(), Arc::new(ctx.clone()));
        send.call_inner(
            &serde_json::json!({"to": "alice", "body": "PR looks good", "task_id": "task-alice-0"}),
        )
        .unwrap();

        // Alice drains her inbox — the reply should cancel the timeout.
        let messages = ctx.drain_inbox(&alice);
        ctx.cancel_timeouts_for_messages(&alice, &messages);

        // The timeout is gone.
        assert_eq!(
            ctx.cancel_timeout(&alice, "task-alice-0"),
            TimeoutCancel::NotFound
        );
    }

    #[tokio::test]
    async fn delegate_default_timeout_used_when_not_specified() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        let tool = DelegateTaskTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(120),
        );
        let result = tool
            .call_inner(&serde_json::json!({"to": "bob", "task": "review PR"}))
            .unwrap();

        // Default timeout (120s) is used when agent doesn't specify timeout_secs.
        assert!(result.contains("120s follow-up timer"));
        assert!(matches!(
            ctx.cancel_timeout(&alice, "task-alice-0"),
            TimeoutCancel::Cancelled { .. }
        ));
    }

    #[tokio::test]
    async fn delegate_rejects_unregistered_target() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        let tool = DelegateTaskTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(30),
        );
        let err = tool
            .call_inner(&serde_json::json!({"to": "ghost", "task": "do something"}))
            .unwrap_err();

        assert!(err.model_message.contains("not a registered team member"));

        // No message should have been pushed.
        assert!(!ctx.has_messages(&AgentId::new("ghost")));
        // No timeout should have been registered.
        assert_eq!(
            ctx.cancel_timeout(&alice, "task-alice-0"),
            TimeoutCancel::NotFound
        );
    }

    #[tokio::test]
    async fn reminder_on_same_task_id_does_not_orphan_delegate_timeout() {
        // Regression: set_reminder used to register its handle under the
        // bare task_id, overwriting the delegate timeout handle. A timely
        // reply then aborted the reminder while the real delegate timeout
        // leaked and fired spuriously (marking the key fired and dropping
        // subsequent replies for that task).
        use crate::tools::set_reminder::SetReminderTool;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");

        // Delegate with a moderate timeout.
        let tool = DelegateTaskTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(300),
        );
        tool.call_inner(&serde_json::json!({"to": "bob", "task": "review PR"}))
            .unwrap();

        // Agent sets a "double insurance" reminder on the same task_id.
        let reminder = SetReminderTool::new(alice.clone(), Arc::new(ctx.clone()));
        reminder
            .call_inner(&serde_json::json!({"reminder":"check on bob","delay_secs":10,"task_id":"task-alice-0"}))
            .unwrap();

        // Bob replies in time.
        let send =
            crate::tools::send_message::SendMessageTool::new(bob.clone(), Arc::new(ctx.clone()));
        send.call_inner(
            &serde_json::json!({"to": "alice", "body": "PR looks good", "task_id": "task-alice-0"}),
        )
        .unwrap();

        // Drain cancels the delegate timeout (the real one, not the
        // reminder's handle).
        let messages = ctx.drain_inbox(&alice);
        ctx.cancel_timeouts_for_messages(&alice, &messages);
        // The delegate timeout is gone; the reminder (peer-less) still
        // pending — a successful peer-less cancel also returns None, so
        // assert behaviorally: after the reply the delegate timeout must
        // not fire even when the clock runs past its deadline.
        assert_eq!(
            ctx.cancel_timeout(&alice, "task-alice-0"),
            TimeoutCancel::NotFound
        );
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(301)).await;
        assert!(
            !ctx.has_messages(&alice),
            "cancelled delegate timeout must not fire"
        );
        // The reminder timer (10s) was left pending by design: cancelling
        // it returns the peer it recorded — here None — proving it was
        // registered and is abortable.
        assert_eq!(
            ctx.cancel_timeout(
                &alice,
                &format!("{}task-alice-0", reserved::REMINDER_KEY_PREFIX)
            ),
            TimeoutCancel::Cancelled { peer: None }
        );
    }

    #[tokio::test]
    async fn delegate_to_operator_is_rejected() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let operator = AgentId::new(reserved::OPERATOR);
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(operator.clone(), reserved::OPERATOR);

        let tool = DelegateTaskTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(30),
        )
        .with_operator(operator.clone());
        let result = tool
            .call_inner(&serde_json::json!({"to": reserved::OPERATOR, "task": "do my job"}))
            .unwrap_err();

        assert!(
            result.model_message.contains(reserved::OPERATOR),
            "error must mention operator: {result}"
        );

        assert!(!ctx.has_messages(&operator));
        // No timeout registered.
        assert_eq!(
            ctx.cancel_timeout(&alice, "task-alice-0"),
            TimeoutCancel::NotFound
        );
    }

    #[tokio::test]
    async fn delegate_to_self_is_rejected() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        let tool = DelegateTaskTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(30),
        );
        let result = tool
            .call_inner(&serde_json::json!({"to": "alice", "task": "review PR"}))
            .unwrap_err();

        assert!(
            result.model_message.contains("yourself"),
            "error must mention self: {result}"
        );
        // No self-message loop: own inbox stays empty.
        assert!(!ctx.has_messages(&alice));
        assert_eq!(
            ctx.cancel_timeout(&alice, "task-alice-0"),
            TimeoutCancel::NotFound
        );
    }

    #[tokio::test]
    async fn delegate_still_works_without_operator_configured() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        let tool = DelegateTaskTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(30),
        );
        let result = tool
            .call_inner(&serde_json::json!({"to": "bob", "task": "review PR"}))
            .unwrap();

        assert!(result.contains("delegated to bob"), "{result}");
        assert!(ctx.has_messages(&bob));
    }
}
