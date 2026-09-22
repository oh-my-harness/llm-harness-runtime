//! `chat_history` tool: query public/team chat and the caller's private chats.

use std::sync::{Arc, LazyLock};

use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

use crate::{AgentId, SocialContext};

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "limit": {
                "type": "integer",
                "description": "Maximum number of matching messages to return.",
                "minimum": 1
            },
            "from": {
                "type": "string",
                "description": "Optional sender ID filter, e.g. \"operator\" or \"planner\"."
            },
            "with": {
                "type": "string",
                "description": "Optional peer ID filter for direct messages."
            },
            "keyword": {
                "type": "string",
                "description": "Optional case-insensitive substring filter."
            }
        },
        "additionalProperties": false
    })
});

/// Tool that lets an agent inspect public chat history and its own private
/// messages without loading the entire conversation into context.
pub struct ChatHistoryTool {
    agent_id: AgentId,
    ctx: Arc<SocialContext>,
}

impl ChatHistoryTool {
    pub fn new(agent_id: AgentId, ctx: Arc<SocialContext>) -> Self {
        Self { agent_id, ctx }
    }

    fn call_inner(&self, args: &serde_json::Value) -> String {
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 100) as usize;
        let from = args.get("from").and_then(serde_json::Value::as_str);
        let with = args.get("with").and_then(serde_json::Value::as_str);
        let keyword = args
            .get("keyword")
            .and_then(serde_json::Value::as_str)
            .map(str::to_lowercase);

        let messages = self
            .ctx
            .chat_history(&self.agent_id, 200)
            .into_iter()
            .filter(|message| from.map_or(true, |from| message.from.as_str() == from))
            .filter(|message| {
                with.map_or(true, |with| {
                    message.from.as_str() == with || message.to.as_str() == with
                })
            })
            .filter(|message| {
                keyword
                    .as_ref()
                    .map_or(true, |needle| message.body.to_lowercase().contains(needle))
            })
            .take(limit)
            .collect::<Vec<_>>();

        if messages.is_empty() {
            return "No matching chat history.".to_string();
        }

        let mut out = format!("Showing {}/{} message(s):\n", messages.len(), limit);
        for message in messages {
            let message_type = match message.message_type {
                crate::MessageType::Broadcast => "group",
                _ => "direct",
            };
            let correlation = message
                .broadcast_id
                .as_ref()
                .or(message.task_id.as_ref())
                .map(|id| format!(" id={id}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "- [{}] {} -> {}{}: {}\n",
                message_type,
                message.from,
                message.to,
                correlation,
                message.body.replace('\n', " ")
            ));
        }
        out
    }
}

impl Tool for ChatHistoryTool {
    fn name(&self) -> &str {
        "chat_history"
    }

    fn label(&self) -> &str {
        "Chat history"
    }

    fn description(&self) -> &str {
        "Read recent team chat history without loading the whole conversation. \
         You can see group messages and direct messages involving you.\n\n\
         Use when: you need to catch up on a group discussion; find a previous \
         instruction; inspect your private thread with one peer.\n\
         Don't use for: sending messages (use send_message); \
         contacting people outside this team.\n\n\
         Examples:\n\
         {}\n\
         {\"limit\": 10, \"from\": \"operator\"}\n\
         {\"limit\": 20, \"with\": \"planner\"}\n\
         {\"limit\": 30, \"keyword\": \"api\"}"
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
            let text = self.call_inner(&args);
            Ok(ToolResult::full(
                vec![DataBlock::text(text)],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

// Pinned to public-API semantics valid at both 9b50310 and WIP state
// (message_id is WIP-only; seeding via register_agent/send_direct/broadcast).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::priority::NORMAL;
    use crate::{AgentId, SocialContext};
    use std::sync::Arc;

    /// A context with one registered recipient ("team") so that broadcasts
    /// actually deliver to someone and land in the shared chat history.
    fn seeded_ctx() -> Arc<SocialContext> {
        let ctx = Arc::new(SocialContext::new(10));
        ctx.register_agent(AgentId::new("team"), "rust");
        ctx
    }

    fn seed_broadcast(ctx: &Arc<SocialContext>, from: &str, n: usize) {
        for i in 0..n {
            ctx.broadcast(&AgentId::new(from), format!("broadcast {i}"))
                .unwrap();
        }
    }

    fn tool_for(ctx: &Arc<SocialContext>, agent: &str) -> ChatHistoryTool {
        ChatHistoryTool::new(AgentId::new(agent), Arc::clone(ctx))
    }

    fn bullet_count(output: &str) -> usize {
        output.lines().filter(|line| line.starts_with("- ")).count()
    }

    #[test]
    fn default_limit_20_returns_newest_20_of_25() {
        let ctx = seeded_ctx();
        seed_broadcast(&ctx, "a", 25);
        let tool = tool_for(&ctx, "b");

        let out = tool.call_inner(&serde_json::json!({}));

        assert_eq!(out.lines().next(), Some("Showing 20/20 message(s):"));
        assert_eq!(bullet_count(&out), 20);
        // History is iterated in reverse: newest broadcast first. Every
        // broadcast carries its own broadcast_id, hence the ` id=` suffix.
        let first = out.lines().nth(1).unwrap();
        assert!(first.starts_with("- [group] a -> team id="));
        assert!(first.ends_with(": broadcast 24"));
        let last = out.lines().last().unwrap();
        assert!(last.starts_with("- [group] a -> team id="));
        assert!(last.ends_with(": broadcast 5"));
    }

    #[test]
    fn limit_clamped_to_1_when_zero() {
        let ctx = seeded_ctx();
        seed_broadcast(&ctx, "a", 5);
        let tool = tool_for(&ctx, "b");

        let out = tool.call_inner(&serde_json::json!({"limit": 0}));

        assert_eq!(out.lines().next(), Some("Showing 1/1 message(s):"));
        assert_eq!(bullet_count(&out), 1);
        let only = out.lines().nth(1).unwrap();
        assert!(only.starts_with("- [group] a -> team id="));
        assert!(only.ends_with(": broadcast 4"));
    }

    #[test]
    fn limit_clamped_to_100_when_101() {
        let ctx = seeded_ctx();
        seed_broadcast(&ctx, "a", 105);
        let tool = tool_for(&ctx, "b");

        let out = tool.call_inner(&serde_json::json!({"limit": 101}));

        assert_eq!(out.lines().next(), Some("Showing 100/100 message(s):"));
        assert_eq!(bullet_count(&out), 100);
    }

    #[test]
    fn from_filter_exact_match() {
        let ctx = seeded_ctx();
        seed_broadcast(&ctx, "operator", 3);
        seed_broadcast(&ctx, "planner", 2);
        let tool = tool_for(&ctx, "b");

        let out = tool.call_inner(&serde_json::json!({"from": "operator"}));

        assert_eq!(out.lines().next(), Some("Showing 3/20 message(s):"));
        assert_eq!(bullet_count(&out), 3);
        for line in out.lines().skip(1) {
            assert!(line.starts_with("- [group] operator -> team id="));
        }
    }

    #[test]
    fn from_is_case_sensitive() {
        let ctx = seeded_ctx();
        seed_broadcast(&ctx, "Operator", 2);
        let tool = tool_for(&ctx, "b");

        let out = tool.call_inner(&serde_json::json!({"from": "operator"}));

        assert_eq!(out, "No matching chat history.");
    }

    #[test]
    fn with_matches_either_side() {
        let ctx = Arc::new(SocialContext::new(10));
        ctx.send_direct(&AgentId::new("a"), &AgentId::new("b"), "hello from a", NORMAL)
            .unwrap();
        ctx.send_direct(&AgentId::new("b"), &AgentId::new("c"), "hello to c", NORMAL)
            .unwrap();
        let tool = tool_for(&ctx, "b");

        let out = tool.call_inner(&serde_json::json!({"with": "a"}));

        assert_eq!(out.lines().next(), Some("Showing 1/20 message(s):"));
        assert_eq!(out.lines().nth(1), Some("- [direct] a -> b: hello from a"));
        assert!(!out.contains("b -> c"));
    }

    #[test]
    fn keyword_case_insensitive() {
        let ctx = seeded_ctx();
        ctx.broadcast(&AgentId::new("a"), "Deploy API gateway").unwrap();
        ctx.broadcast(&AgentId::new("a"), "standup notes").unwrap();
        let tool = tool_for(&ctx, "b");

        let upper = tool.call_inner(&serde_json::json!({"keyword": "API"}));
        let lower = tool.call_inner(&serde_json::json!({"keyword": "api"}));

        assert_eq!(upper, lower);
        assert_eq!(bullet_count(&upper), 1);
        assert!(upper.contains("Deploy API gateway"));
        assert!(!upper.contains("standup"));
    }

    #[test]
    fn combined_filters_intersect() {
        let ctx = seeded_ctx();
        ctx.broadcast(&AgentId::new("b"), "Deploy API gateway now").unwrap();
        ctx.broadcast(&AgentId::new("operator"), "standup notes today")
            .unwrap();
        ctx.send_direct(&AgentId::new("b"), &AgentId::new("a"), "API reply", NORMAL)
            .unwrap();
        ctx.send_direct(
            &AgentId::new("operator"),
            &AgentId::new("b"),
            "Deploy API gateway privately",
            NORMAL,
        )
        .unwrap();
        let tool = tool_for(&ctx, "b");

        let out = tool.call_inner(&serde_json::json!({"from": "b", "with": "b", "keyword": "api"}));

        assert_eq!(out.lines().next(), Some("Showing 2/20 message(s):"));
        assert_eq!(bullet_count(&out), 2);
        // Newest first: the b->a direct was pushed after the b broadcast.
        assert_eq!(out.lines().nth(1), Some("- [direct] b -> a: API reply"));
        let broadcast_hit = out.lines().nth(2).unwrap();
        assert!(broadcast_hit.starts_with("- [group] b -> team id="));
        assert!(broadcast_hit.ends_with(": Deploy API gateway now"));
        assert!(!out.contains("standup"));
        assert!(!out.contains("privately"));
    }

    #[test]
    fn privacy_boundary_excludes_strangers_direct() {
        let ctx = seeded_ctx();
        ctx.broadcast(&AgentId::new("x"), "public announcement").unwrap();
        ctx.send_direct(&AgentId::new("x"), &AgentId::new("y"), "secret private note", NORMAL)
            .unwrap();
        let tool = tool_for(&ctx, "b");

        let out = tool.call_inner(&serde_json::json!({}));

        assert!(out.contains("public announcement"));
        assert!(!out.contains("secret private note"));
        assert_eq!(bullet_count(&out), 1);
    }

    #[test]
    fn message_type_annotations_and_empty_message() {
        let ctx = seeded_ctx();
        let bid = ctx.broadcast(&AgentId::new("a"), "hello team").unwrap();
        ctx.send_direct(&AgentId::new("a"), &AgentId::new("b"), "psst", NORMAL)
            .unwrap();
        let tool = tool_for(&ctx, "b");

        let out = tool.call_inner(&serde_json::json!({}));

        assert_eq!(out.lines().next(), Some("Showing 2/20 message(s):"));
        assert_eq!(out.lines().nth(1), Some("- [direct] a -> b: psst"));
        assert_eq!(
            out.lines().nth(2),
            Some(format!("- [group] a -> team id={bid}: hello team").as_str())
        );

        let empty_tool = tool_for(&Arc::new(SocialContext::new(10)), "b");
        assert_eq!(
            empty_tool.call_inner(&serde_json::json!({})),
            "No matching chat history."
        );
    }

    #[test]
    fn limit_negative_falls_back_to_default() {
        let ctx = seeded_ctx();
        seed_broadcast(&ctx, "a", 25);
        let tool = tool_for(&ctx, "b");

        let out = tool.call_inner(&serde_json::json!({"limit": -1}));

        assert_eq!(out.lines().next(), Some("Showing 20/20 message(s):"));
        assert_eq!(bullet_count(&out), 20);
    }
}
