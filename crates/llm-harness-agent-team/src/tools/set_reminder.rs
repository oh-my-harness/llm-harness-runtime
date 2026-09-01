//! `set_reminder` tool: schedule a self-reminder.
//!
//! The agent sets a reminder that fires after a specified delay. When the
//! timer expires, a `MessageType::SelfReminder` message is pushed into the
//! agent's inbox, which is delivered via the normal steer / idle-wake
//! pipeline.
//!
//! This is the agent-facing counterpart to `TimeoutManager`. It lets the
//! agent say "remind me to check on X in 5 minutes" — e.g. after delegating
//! a task and wanting to follow up if no result arrives.

use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

use crate::timeout_manager::TimeoutManager;
use crate::{AgentId, SocialContext};

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "reminder": {
                "type": "string",
                "description": "The reminder message to deliver to yourself. e.g. \"Check if researcher-1 finished the analysis\""
            },
            "delay_secs": {
                "type": "integer",
                "description": "Number of seconds to wait before the reminder fires. e.g. 300 for 5 minutes.",
                "minimum": 1
            },
            "task_id": {
                "type": "string",
                "description": "Optional task correlation ID. If provided, the reminder message will carry this ID so you can match it to the original task."
            }
        },
        "required": ["reminder", "delay_secs"],
        "additionalProperties": false
    })
});

/// Tool that schedules a self-reminder via [`TimeoutManager`].
pub struct SetReminderTool {
    agent_id: AgentId,
    ctx: Arc<SocialContext>,
}

impl SetReminderTool {
    /// Schedule reminders for `agent_id` via the shared social context's
    /// timeout registry.
    pub fn new(agent_id: AgentId, ctx: Arc<SocialContext>) -> Self {
        Self { agent_id, ctx }
    }

    // ToolFailure is ~144 bytes; the harness Tool trait boxes the future so
    // its own signatures never trip this lint. Mirrors failure_breaker.rs.
    #[allow(clippy::result_large_err)]
    pub(crate) fn call_inner(&self, input: &serde_json::Value) -> Result<String, ToolFailure> {
        let reminder = input
            .get("reminder")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolFailure::invalid_arguments("missing string field 'reminder'"))?;
        let delay_secs = match input.get("delay_secs").and_then(serde_json::Value::as_u64) {
            Some(n) if n > 0 => n,
            _ => {
                return Err(ToolFailure::invalid_arguments(
                    "'delay_secs' must be a positive integer.",
                ));
            }
        };
        let task_id = input
            .get("task_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);

        let timer = TimeoutManager::new((*self.ctx).clone());
        let replaced = timer
            .set_self_reminder_durably(
                self.agent_id.clone(),
                reminder.to_string(),
                Duration::from_secs(delay_secs),
                task_id,
            )
            .map_err(|error| {
                ToolFailure::new(
                    "timer_journal_persistence_failed",
                    format!("failed to persist reminder: {error}"),
                )
            })?;

        let mut msg = format!(
            "Self-reminder scheduled: in {}s you will be reminded: \"{reminder}\"",
            delay_secs
        );
        if replaced {
            msg.push_str(" (replaced a pending reminder)");
        }
        Ok(msg)
    }
}

impl Tool for SetReminderTool {
    fn name(&self) -> &str {
        "set_reminder"
    }

    fn label(&self) -> &str {
        "Schedule self-reminder"
    }

    fn description(&self) -> &str {
        "Schedule a reminder to yourself that fires after a delay. \
         When the timer expires, the reminder message is delivered to your inbox \
         and injected into your next turn via the steer mechanism.\n\n\
         Use when: waiting for a delegated task result and want to follow up; \
         pacing work across multiple steps with time gaps; \
         reminding yourself to re-check a condition later.\n\n\
         The reminder is a one-shot timer — it fires once. If you need recurring \
         reminders, schedule a new one after each fires.\n\n\
         Examples:\n\
         {\"reminder\": \"Check if researcher-1 finished the analysis\", \"delay_secs\": 300}\n\
         {\"reminder\": \"Re-broadcast the question if no response\", \"delay_secs\": 120, \"task_id\": \"task-42\"}"
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

    #[tokio::test(start_paused = true)]
    async fn schedules_self_reminder() {
        // Tool-level: parameter validation and scheduling confirmation.
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        let tool = SetReminderTool::new(alice.clone(), Arc::new(ctx.clone()));

        let result = tool
            .call_inner(
                &serde_json::json!({"reminder":"check results","delay_secs":1,"task_id":"t1"}),
            )
            .unwrap();

        assert!(result.contains("Self-reminder scheduled"));
        assert!(result.contains("1s"));

        // The reminder is registered under its namespaced key immediately.
        // Peer-less timeout: cancel returns None even on success, so
        // verify registration behaviorally — cancel it and let the clock
        // run past the delay: the reminder message must never arrive.
        assert_eq!(
            ctx.cancel_timeout(&alice, &format!("{}t1", reserved::REMINDER_KEY_PREFIX)),
            TimeoutCancel::Cancelled { peer: None }
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(
            ctx.drain_inbox(&alice).is_empty(),
            "cancelled reminder must not fire"
        );
    }

    #[tokio::test]
    async fn timer_journal_failure_is_explicit() {
        let temp = tempfile::tempdir().unwrap();
        let timer_path = temp.path().join("timers.sqlite3");
        let timer_journal = crate::timer_journal::TimeoutJournal::open(&timer_path).unwrap();
        let ctx = SocialContext::with_journal(10, None, Some(Arc::new(timer_journal))).unwrap();
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        std::fs::write(&timer_path, b"not a sqlite database").unwrap();
        let tool = SetReminderTool::new(alice, Arc::new(ctx));
        let error = tool
            .call_inner(&serde_json::json!({"reminder":"check results","delay_secs":60}))
            .unwrap_err();

        assert_eq!(error.code, "timer_journal_persistence_failed");
        assert!(error.to_string().contains("failed to persist reminder"));
    }

    #[tokio::test(start_paused = true)]
    async fn set_reminder_tool_mentions_replacement() {
        // A second set_reminder with the same task_id replaces the still-
        // pending first reminder; the tool output must say so, while a
        // fresh reminder must not claim a replacement.
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        let tool = SetReminderTool::new(alice.clone(), Arc::new(ctx.clone()));

        let fresh = tool
            .call_inner(
                &serde_json::json!({"reminder":"check results","delay_secs":60,"task_id":"t1"}),
            )
            .unwrap();
        assert!(
            !fresh.contains("(replaced a pending reminder)"),
            "fresh reminder must not claim replacement: {fresh}"
        );

        let replaced = tool
            .call_inner(
                &serde_json::json!({"reminder":"check again","delay_secs":60,"task_id":"t1"}),
            )
            .unwrap();
        assert!(
            replaced.contains("(replaced a pending reminder)"),
            "replacing a pending reminder must be surfaced: {replaced}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reminder_fires_message_after_delay() {
        // Fire-path: paused time makes the delay deterministic — no real
        // sleeping. Delivery is observed through the agent's inbox.
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        let tm = TimeoutManager::new(ctx.clone());
        let _replaced = tm.set_self_reminder(
            alice.clone(),
            "check results",
            Duration::from_secs(1),
            Some("t1".to_string()),
        );

        assert!(
            !ctx.has_messages(&alice),
            "nothing before the delay elapses"
        );

        tokio::time::advance(Duration::from_secs(1)).await;
        ctx.wait_for_message(&alice).await;

        let msgs = ctx.drain_inbox(&alice);
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].message_type,
            crate::inbox::MessageType::SelfReminder
        );
        assert_eq!(msgs[0].task_id.as_deref(), Some("t1"));
        // Fired self-reminders deregister themselves (no late-reply filter).
        assert_eq!(
            ctx.cancel_timeout(&alice, &format!("{}t1", reserved::REMINDER_KEY_PREFIX)),
            TimeoutCancel::NotFound
        );
    }

    #[test]
    fn missing_reminder_returns_error() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let tool = SetReminderTool::new(alice, Arc::new(ctx));

        let result = tool
            .call_inner(&serde_json::json!({"delay_secs":10}))
            .unwrap_err();

        assert!(result.model_message.contains("reminder"));
    }

    #[test]
    fn zero_delay_returns_error() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let tool = SetReminderTool::new(alice, Arc::new(ctx));

        let result = tool
            .call_inner(&serde_json::json!({"reminder":"x","delay_secs":0}))
            .unwrap_err();

        assert!(result.model_message.contains("must be a positive integer"));
    }
}
