//! `search_contacts` tool: list the calling agent's contact list.

use std::sync::LazyLock;

use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

use crate::{AgentId, SocialContext};

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "limit": {
                "type": "integer",
                "description": "Maximum number of contacts to return. Omit to list all.",
                "minimum": 1
            }
        },
        "additionalProperties": false
    })
});

/// Tool that lists the calling agent's contact list.
pub struct SearchContactsTool {
    agent_id: AgentId,
    ctx: std::sync::Arc<SocialContext>,
}

impl SearchContactsTool {
    /// Read the contact list of `agent_id` from the shared social context.
    pub fn new(agent_id: AgentId, ctx: std::sync::Arc<SocialContext>) -> Self {
        Self { agent_id, ctx }
    }

    pub(crate) fn call_inner(&self, limit: Option<usize>) -> String {
        let mut contacts = self.ctx.list_contacts(&self.agent_id);
        if contacts.is_empty() {
            return "No contacts found.".to_string();
        }
        let total = contacts.len();
        // list_contacts is strength-sorted at the source (ContactIndex);
        // truncate keeps the strongest contacts.
        if let Some(n) = limit {
            contacts.truncate(n);
        }
        let mut out = format!("Showing {}/{} contact(s):\n", contacts.len(), total);
        for c in &contacts {
            out.push_str(&format!(
                "- {} (expertise: {}, strength: {:.2})\n",
                c.peer_agent_id, c.peer_expertise, c.strength
            ));
        }
        out
    }
}

impl Tool for SearchContactsTool {
    fn name(&self) -> &str {
        "search_contacts"
    }

    fn label(&self) -> &str {
        "List contacts"
    }

    fn description(&self) -> &str {
        "List contacts in your contact list with their agent ID, \
         expertise, and relationship strength. You decide who is the \
         best match — no query needed.\n\n\
         Use when: you need to find a peer with specific expertise; \
         deciding who to delegate or message to; seeing who you know.\n\
         Don't use for: sending messages (use send_message); \
         broadcasting to everyone (use broadcast).\n\
         Returns: Your contact list. Empty if you have no contacts.\n\n\
         Examples:\n\
         {}\n\
         {\"limit\": 5}"
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
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            let text = self.call_inner(limit);
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
    use crate::SocialContext;

    #[test]
    fn list_contacts_returns_empty_for_new_agent() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        let tool = SearchContactsTool::new(alice, std::sync::Arc::new(ctx));
        let result = tool.call_inner(None);
        assert!(result.contains("No contacts found"));
    }

    #[test]
    fn list_contacts_shows_all_fields() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python data science");

        // Alice talks to bob — bob becomes a contact.
        ctx.send_direct(&alice, &bob, "hi", 0);

        let tool = SearchContactsTool::new(alice, std::sync::Arc::new(ctx));
        let result = tool.call_inner(None);
        assert!(result.contains("bob"));
        assert!(result.contains("python data science"));
        assert!(result.contains("strength"));
    }

    #[test]
    fn list_contacts_respects_limit() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        let carol = AgentId::new("carol");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.register_agent(carol.clone(), "go");

        ctx.send_direct(&alice, &bob, "hi bob", 0);
        ctx.send_direct(&alice, &carol, "hi carol", 0);

        let tool = SearchContactsTool::new(alice, std::sync::Arc::new(ctx));
        let result = tool.call_inner(Some(1));
        assert!(result.contains("1/2"));
    }
}
