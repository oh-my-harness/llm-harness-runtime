//! `broadcast` tool: send a message to all agents in the team.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

use crate::timeout_manager::TimeoutManager;
use crate::{AgentId, SocialContext};

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "The broadcast message body to send to all team members. e.g. \"Who can help with Rust?\""
            },
            "timeout_secs": {
                "type": "integer",
                "description": "How long (in seconds) to wait for volunteer replies before being nudged. If omitted, uses the system default. Set a shorter timeout for urgent requests, longer if agents may be slow to respond.",
                "minimum": 1
            }
        },
        "required": ["message"],
        "additionalProperties": false
    })
});
/// Tool that broadcasts a message to all team members.
///
/// After broadcasting, schedules a cancellable timeout via
/// [`TimeoutManager`] keyed by the `broadcast_id`. When a volunteer reply
/// carrying that `broadcast_id` is drained by the runner, the timeout is
/// cancelled. If it fires, the agent is nudged to follow up.
pub struct BroadcastTool {
    agent_id: AgentId,
    ctx: Arc<SocialContext>,
    timeout: Duration,
}

impl BroadcastTool {
    /// Broadcast on behalf of `agent_id` through the shared social context,
    /// with each awaited reply bounded by `timeout`.
    pub fn new(agent_id: AgentId, ctx: Arc<SocialContext>, timeout: Duration) -> Self {
        Self {
            agent_id,
            ctx,
            timeout,
        }
    }

    // ToolFailure is ~144 bytes; the harness Tool trait boxes the future so
    // its own signatures never trip this lint. Mirrors failure_breaker.rs.
    #[allow(clippy::result_large_err)]
    pub(crate) fn call_inner(&self, input: &serde_json::Value) -> Result<String, ToolFailure> {
        let body = input
            .get("message")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolFailure::invalid_arguments("missing string field 'message'"))?;

        let broadcast_id = self.ctx.broadcast(&self.agent_id, body);

        // Schedule a cancellable timeout keyed by the broadcast_id. When a
        // volunteer reply carrying that broadcast_id is drained, the runner
        // cancels this timeout. If it fires, the agent is nudged to follow up.
        // The agent can override the default timeout via timeout_secs.
        let timeout = input
            .get("timeout_secs")
            .and_then(super::parse_timeout_secs)
            .unwrap_or(self.timeout);
        let timer = TimeoutManager::new((*self.ctx).clone());
        timer.set_timeout(
            self.agent_id.clone(),
            format!(
                "Your broadcast ({broadcast_id}) timed out — no one replied within {} \
                 seconds. Consider re-broadcasting, delegating directly, or proceeding \
                 without help.",
                timeout.as_secs()
            ),
            timeout,
            Some(broadcast_id.clone()),
            None,
        );
        let timeout_msg = format!(
            " A {}s timeout is set — you'll be nudged if no one replies.",
            timeout.as_secs()
        );

        Ok(format!(
            "Broadcast {broadcast_id} sent to all team members: {body}{timeout_msg}"
        ))
    }
}

impl Tool for BroadcastTool {
    fn name(&self) -> &str {
        "broadcast"
    }

    fn label(&self) -> &str {
        "Broadcast message"
    }

    fn description(&self) -> &str {
        "Broadcast a message to all agents in the team. \
         Every agent except you will receive it in their inbox.\n\n\
         Two-phase volunteer protocol: when you broadcast a request for help \
         (e.g. \"Who can help with X?\"), agents who can help will reply with a \
         short \"I can help\" message carrying your broadcast_id. You will see \
         their replies aggregated as a volunteer list. Pick one and use \
         delegate_task to assign the work — do not expect volunteers to start \
         working on their own. Agents who cannot help will simply ignore the \
         broadcast.\n\n\
         Use when: asking who has specific expertise; soliciting volunteers; \
         announcing information to the whole team.\n\
         Don't use for: messaging one specific agent (use send_message); \
         delegating a tracked task (use delegate_task).\n\
         After broadcasting, you will wait for volunteer responses. If no one \
         replies in time, you will receive a timeout notification — then \
         decide whether to re-broadcast, delegate directly, or proceed without \
         help. You can specify timeout_secs to adjust the wait — shorter for \
         urgent requests, longer if agents may be slow to respond.\n\n\
         {\"message\": \"Who can help with database optimization?\", \"timeout_secs\": 120}\n\
         {\"message\": \"Heads up: deploying in 10 minutes.\"}"
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

    #[tokio::test]
    async fn broadcast_with_timeout_sets_cancellable_timer() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");

        let tool = BroadcastTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(30),
        );

        let result = tool
            .call_inner(&serde_json::json!({"message": "who can help?"}))
            .unwrap();

        // Tool reports the timeout.
        assert!(result.contains("30s timeout is set"));

        // The timeout is registered and cancellable by broadcast_id.
        // Drain bob's inbox to get the broadcast message with broadcast_id.
        let bob_msgs = ctx.drain_inbox(&bob);
        assert_eq!(bob_msgs.len(), 1);
        let broadcast_id = bob_msgs[0].broadcast_id.as_ref().unwrap();
        // Peer-less timeout: a successful cancel yields
        // `Cancelled { peer: None }` — the peer field does not signal
        // success. Assert behaviorally: cancel, run the clock past the
        // deadline, verify no timeout message lands.
        assert_eq!(
            ctx.cancel_timeout(&alice, broadcast_id),
            TimeoutCancel::Cancelled { peer: None }
        );
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(
            ctx.drain_inbox(&alice).is_empty(),
            "cancelled broadcast timeout must not fire"
        );
    }

    #[tokio::test]
    async fn broadcast_agent_specified_timeout_overrides_default() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");

        let tool = BroadcastTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(60),
        );
        let result = tool
            .call_inner(&serde_json::json!({"message": "who can help?", "timeout_secs": 15}))
            .unwrap();

        // Agent-specified timeout (15s) overrides the default (60s).
        assert!(result.contains("15s timeout is set"));
        assert!(!result.contains("60s"));
    }

    #[tokio::test]
    async fn broadcast_default_timeout_used_when_not_specified() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");

        let tool = BroadcastTool::new(alice.clone(), Arc::new(ctx), Duration::from_secs(45));
        let result = tool
            .call_inner(&serde_json::json!({"message": "standup"}))
            .unwrap();

        // Default timeout (45s) is used when agent doesn't specify timeout_secs.
        assert!(result.contains("45s timeout is set"));
    }
}
