//! Social tools: per-agent `Tool` impls for team interaction.
//!
//! `SocialToolPlugin` bundles all tools and registers them into a
//! harness via the `Plugin` trait.
pub mod broadcast;
pub mod delegate_task;
pub mod search_contacts;
pub mod send_message;
pub mod set_reminder;

use std::sync::Arc;
use std::time::Duration;

use llm_harness_agent::Plugin;
use llm_harness_types::Tool;

use crate::team::TeamConfig;
use crate::{AgentId, SocialContext};
pub use broadcast::BroadcastTool;
pub use delegate_task::DelegateTaskTool;
use llm_harness_strategy::status_panel::{TodoStore, TodoWriteTool};
pub use search_contacts::SearchContactsTool;
pub use send_message::SendMessageTool;
pub use set_reminder::SetReminderTool;

/// Plugin that registers all social tools for a specific agent.
pub struct SocialToolPlugin {
    agent_id: AgentId,
    ctx: Arc<SocialContext>,
    todo_store: TodoStore,
    broadcast_timeout: Duration,
    delegate_timeout: Duration,
    /// Operator id, when configured; `None` disables the operator check.
    operator_id: Option<AgentId>,
}

/// Parse the `timeout_secs` tool argument: a positive integer number of
/// seconds. Zero, negatives, and non-numeric values fall back to `None`
/// (the caller then uses its configured default).
pub(crate) fn parse_timeout_secs(v: &serde_json::Value) -> Option<Duration> {
    v.as_u64().filter(|&n| n > 0).map(Duration::from_secs)
}

impl SocialToolPlugin {
    /// Create a new plugin that registers the social tools acting on behalf
    /// of `agent_id` through the shared `ctx`.
    pub fn new(agent_id: AgentId, ctx: SocialContext, todo_store: TodoStore) -> Self {
        Self {
            agent_id,
            ctx: Arc::new(ctx),
            todo_store,
            broadcast_timeout: TeamConfig::DEFAULT_BROADCAST_TIMEOUT,
            delegate_timeout: TeamConfig::DEFAULT_DELEGATE_TIMEOUT,
            operator_id: None,
        }
    }

    /// Configured broadcast timeout (exposed for tests).
    #[cfg(test)]
    pub(crate) fn broadcast_timeout(&self) -> Duration {
        self.broadcast_timeout
    }

    /// Configured delegate follow-up timeout (exposed for tests).
    #[cfg(test)]
    pub(crate) fn delegate_timeout(&self) -> Duration {
        self.delegate_timeout
    }

    /// Set the broadcast timeout (time to wait for volunteer replies).
    pub fn with_broadcast_timeout(mut self, timeout: Duration) -> Self {
        self.broadcast_timeout = timeout;
        self
    }

    /// Set the delegate follow-up timeout (time to wait for a delegate's reply).
    pub fn with_delegate_timeout(mut self, timeout: Duration) -> Self {
        self.delegate_timeout = timeout;
        self
    }

    /// Set the operator id; delegating to the operator is then rejected.
    pub fn with_operator(mut self, id: AgentId) -> Self {
        self.operator_id = Some(id);
        self
    }
}

impl Plugin for SocialToolPlugin {
    fn name(&self) -> &str {
        "social-tools"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        // todo_write — always available; the agent needs it to manage work.
        tools.push(Arc::new(TodoWriteTool::new(self.todo_store.clone())));

        // set_reminder — always available; lets the agent schedule self-reminders.
        tools.push(Arc::new(SetReminderTool::new(
            self.agent_id.clone(),
            self.ctx.clone(),
        )));

        tools.push(Arc::new(SearchContactsTool::new(
            self.agent_id.clone(),
            self.ctx.clone(),
        )));

        tools.push(Arc::new(SendMessageTool::new(
            self.agent_id.clone(),
            self.ctx.clone(),
        )));

        // broadcast — with cancellable timeout.
        let broadcast = BroadcastTool::new(
            self.agent_id.clone(),
            self.ctx.clone(),
            self.broadcast_timeout,
        );
        tools.push(Arc::new(broadcast));

        // delegate_task — with cancellable follow-up timeout.
        let delegate = DelegateTaskTool::new(
            self.agent_id.clone(),
            self.ctx.clone(),
            self.delegate_timeout,
        );
        let delegate = match &self.operator_id {
            Some(op) => delegate.with_operator(op.clone()),
            None => delegate,
        };
        tools.push(Arc::new(delegate));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn search_contacts_returns_empty_for_new_agent() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        let store = TodoStore::new();
        let plugin = SocialToolPlugin::new(alice, ctx, store);
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        plugin.register_tools(&mut tools);

        // search_contacts should be registered.
        assert!(tools.iter().any(|t| t.name() == "search_contacts"));
    }

    #[tokio::test]
    async fn broadcast_reaches_all_agents() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        let carol = AgentId::new("carol");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.register_agent(carol.clone(), "go");

        let tool = BroadcastTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(60),
        );
        let result = tool
            .call_inner(&json!({"message": "team standup in 5 min"}))
            .unwrap();
        assert!(result.starts_with("Broadcast "));
        assert!(result.contains("sent to all team members"));

        // Sender does not receive the broadcast.
        assert!(!ctx.has_messages(&alice));
        // All other agents receive it.
        assert!(ctx.has_messages(&bob));
        assert!(ctx.has_messages(&carol));
        assert_eq!(ctx.drain_inbox(&bob).len(), 1);
        assert_eq!(ctx.drain_inbox(&carol).len(), 1);
    }

    #[test]
    fn send_message_delivers_to_target_inbox() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        let tool = SendMessageTool::new(alice.clone(), Arc::new(ctx.clone()));
        let input = json!({"to": "bob", "body": "hello from alice"});
        let result = tool.call_inner(&input).unwrap();
        assert!(result.contains("Message sent to bob"));
    }

    #[test]
    fn call_inner_malformed_typed_json_returns_typed_error() {
        // 13.1 (C1): malformed typed JSON must surface through the typed
        // error channel — `Err(ToolFailure)`, code `invalid_arguments` —
        // never as an "Invalid JSON input: …" success payload.
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        let broadcast = BroadcastTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(60),
        );
        let send = SendMessageTool::new(alice.clone(), Arc::new(ctx.clone()));
        let delegate = DelegateTaskTool::new(
            alice.clone(),
            Arc::new(ctx.clone()),
            Duration::from_secs(60),
        );
        let reminder = SetReminderTool::new(alice.clone(), Arc::new(ctx));

        let err = broadcast.call_inner(&json!({"message": 123})).unwrap_err();
        assert_eq!(err.code, "invalid_arguments");

        let err = send
            .call_inner(&json!({"to": 123, "body": "hi"}))
            .unwrap_err();
        assert_eq!(err.code, "invalid_arguments");

        let err = delegate
            .call_inner(&json!({"to": 123, "task": "x"}))
            .unwrap_err();
        assert_eq!(err.code, "invalid_arguments");

        let err = reminder
            .call_inner(&json!({"reminder": 123, "delay_secs": 10}))
            .unwrap_err();
        assert_eq!(err.code, "invalid_arguments");
    }

    #[test]
    fn plugin_defaults_match_teamconfig_constants() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let plugin = SocialToolPlugin::new(alice, ctx, TodoStore::new());
        assert_eq!(
            plugin.broadcast_timeout(),
            TeamConfig::DEFAULT_BROADCAST_TIMEOUT
        );
        assert_eq!(
            plugin.delegate_timeout(),
            TeamConfig::DEFAULT_DELEGATE_TIMEOUT
        );
    }

    #[test]
    fn parse_timeout_secs_rejects_zero_and_nonpositive() {
        assert_eq!(parse_timeout_secs(&json!(0)), None);
        assert_eq!(parse_timeout_secs(&json!(-1)), None);
        assert_eq!(parse_timeout_secs(&json!("3")), None);
        assert_eq!(parse_timeout_secs(&json!(5)), Some(Duration::from_secs(5)));
    }
}
