//! Timeout manager: schedules timeout messages that get pushed into an agent's
//! inbox after a specified duration.
//!
//! Similar to a human setting a reminder: "If I don't hear back in 30 minutes,
//! check again." The timeout message enters the same [`InboxStore`] priority
//! queue as all other messages, so it's handled by the same injection paths
//! (idle wake or monitor task → steer).

use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::debug;

use crate::AgentId;
use crate::inbox::{InboxMessage, MessageType};
use crate::social_context::SocialContext;

/// Manages timeout messages for agents.
///
/// Each call to [`Self::set_timeout`] spawns a tokio task that sleeps for the
/// specified duration, then pushes a `MessageType::Timeout` message into the
/// agent's inbox. The message is handled by the normal inbox → steer/idle-wake
/// pipeline.
#[derive(Clone)]
pub struct TimeoutManager {
    ctx: SocialContext,
}

impl TimeoutManager {
    /// Create a new timeout manager backed by the given social context.
    pub fn new(ctx: SocialContext) -> Self {
        Self { ctx }
    }

    /// Schedule a timeout message.
    ///
    /// After `after` duration, a `MessageType::Timeout` message with the given
    /// `body` is pushed into `agent_id`'s inbox. If `task_id` is provided, it
    /// is attached to the message so the agent can correlate it with the
    /// original task.
    ///
    /// When a delegation timeout (`peer: Some(_)`) fires, the collaboration
    /// is treated as failed: a `FailedCollaboration` relation event is
    /// applied from `agent_id` against the peer, weakening that bond.
    /// Peer-less timeouts (broadcast, key-less) emit no relation event.
    ///
    /// Returns a `JoinHandle` that can be awaited or aborted.
    pub fn set_timeout(
        &self,
        agent_id: AgentId,
        body: impl Into<String>,
        after: Duration,
        task_id: Option<String>,
        peer: Option<AgentId>,
    ) -> JoinHandle<()> {
        let ctx = self.ctx.clone();
        let register_ctx = self.ctx.clone();
        let body = body.into();
        let register_key = task_id.clone();
        let cleanup_key = task_id.clone();
        let register_agent = agent_id.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(after).await;
            ctx.push_message(InboxMessage {
                from: AgentId::new(reserved::TIMER),
                to: agent_id.clone(),
                body,
                message_type: MessageType::Timeout,
                priority: crate::inbox::priority::TIMEOUT,
                task_id,
                broadcast_id: None,
            });
            if let Some(k) = cleanup_key {
                // Some(peer) ⇒ a delegation timed out: the collaboration
                // failed, weakening the bond toward the delegate. If the
                // timeout was cancelled before fire, mark_fired returns
                // None and no relation event is emitted.
                if let Some(peer) = ctx.mark_timeout_fired(&agent_id, &k) {
                    ctx.apply_relation_event(
                        &agent_id,
                        &peer,
                        crate::relation::RelationEvent::FailedCollaboration,
                    );
                }
            }
            debug!(agent = %agent_id, "timeout message pushed to inbox");
        });

        // Register the timeout so a timely reply can cancel it. `peer` is
        // returned by cancel/mark_fired for relation events (delegation
        // only; broadcast/self-reminder timeouts are peer-less).
        if let Some(k) = register_key {
            // The replacement flag is a self-reminder concern: a delegation
            // timeout re-registered under its own correlation id is bookkeeping
            // internal to one request, never an LLM-visible replacement, so the
            // bool is deliberately discarded here.
            let _ = register_ctx.register_timeout(&register_agent, k, peer, handle.abort_handle());
        }
        handle
    }

    /// Schedule a self-reminder message.
    ///
    /// After `after` duration, a `MessageType::SelfReminder` message with the
    /// given `body` is pushed into `agent_id`'s inbox. The priority is lower
    /// than a timeout (30 vs 50) — reminders are important but not urgent.
    ///
    /// Returns `true` if a pending reminder (or timeout) was replaced under
    /// the reminder's key — the previous timer task has been aborted — and
    /// `false` if the reminder is fresh. Reminders without a `task_id`
    /// register nothing and always return `false`.
    ///
    /// The spawned timer task cannot be awaited through this method; tests
    /// observe delivery behaviorally via the agent's inbox.
    pub fn set_self_reminder(
        &self,
        agent_id: AgentId,
        body: impl Into<String>,
        after: Duration,
        task_id: Option<String>,
    ) -> bool {
        let ctx = self.ctx.clone();
        let register_ctx = self.ctx.clone();
        let body = body.into();
        // Register under a namespaced key: self-reminders share the
        // TimeoutRegistry with delegate/broadcast timeouts, and a bare
        // task_id here would overwrite (and orphan) a pending delegation
        // timeout with the same key — the reminder handle would then be
        // aborted by the reply cancellation while the real timeout leaks
        // and fires spuriously.
        let register_key = task_id
            .as_ref()
            .map(|k| format!("{}{k}", reserved::REMINDER_KEY_PREFIX));
        let cleanup_key = register_key.clone();
        let register_agent = agent_id.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(after).await;
            ctx.push_message(InboxMessage {
                from: AgentId::new(reserved::SELF),
                to: agent_id.clone(),
                body,
                message_type: MessageType::SelfReminder,
                priority: crate::inbox::priority::SELF_REMINDER,
                task_id,
                broadcast_id: None,
            });
            if let Some(k) = cleanup_key {
                ctx.remove_timeout(&agent_id, &k);
            }
            debug!(agent = %agent_id, "self-reminder pushed to inbox");
        });
        if let Some(k) = register_key {
            register_ctx.register_timeout(&register_agent, k, None, handle.abort_handle())
        } else {
            false
        }
    }
}

/// Reserved synthetic sender ids used by the timeout subsystem.
pub mod reserved {
    /// Synthetic sender of [`MessageType::Timeout`] messages.
    pub const TIMER: &str = "timer";
    /// Synthetic sender of [`MessageType::SelfReminder`] messages.
    pub const SELF: &str = "self";
    /// Synthetic sender for operator-drained inboxes; also the id of the
    /// team's human operator inbox (see [`crate::team::AgentTeam`]).
    pub const OPERATOR: &str = "operator";
    /// Prefix for self-reminder timeout registry keys.
    pub const REMINDER_KEY_PREFIX: &str = "reminder:";

    /// Returns `true` if `id` collides with a reserved id: the synthetic
    /// senders themselves, or a self-reminder registry-key prefix.
    pub fn is_reserved_id(id: &str) -> bool {
        id == TIMER || id == SELF || id == OPERATOR || id.starts_with(REMINDER_KEY_PREFIX)
    }
}

#[cfg(test)]
mod reserved_tests {
    use super::reserved;

    #[test]
    fn reserved_constants_pin() {
        assert_eq!(reserved::TIMER, "timer");
        assert_eq!(reserved::SELF, "self");
        assert_eq!(reserved::OPERATOR, "operator");
        assert_eq!(reserved::REMINDER_KEY_PREFIX, "reminder:");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timeout_pushes_message_after_duration() {
        let ctx = SocialContext::new(10);
        let agent = AgentId::new("alpha");
        ctx.register_agent(agent.clone(), "testing");

        let tm = TimeoutManager::new(ctx.clone());
        let handle = tm.set_timeout(
            agent.clone(),
            "30 seconds elapsed",
            Duration::from_millis(50),
            Some("task-1".to_string()),
            None,
        );

        // Initially no messages.
        assert!(!ctx.has_messages(&agent));

        // Wait for the timeout to fire.
        handle.await.unwrap();

        // Now the inbox should have the timeout message.
        assert!(ctx.has_messages(&agent));

        let msgs = ctx.drain_inbox(&agent);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].message_type, MessageType::Timeout);
        assert_eq!(msgs[0].body, "30 seconds elapsed");
        assert_eq!(msgs[0].task_id, Some("task-1".to_string()));
        assert_eq!(msgs[0].priority, 50);
    }

    #[tokio::test]
    async fn timeout_can_be_aborted() {
        let ctx = SocialContext::new(10);
        let agent = AgentId::new("beta");
        ctx.register_agent(agent.clone(), "testing");

        let tm = TimeoutManager::new(ctx.clone());
        let handle = tm.set_timeout(
            agent.clone(),
            "should never fire",
            Duration::from_secs(10),
            None,
            None,
        );

        // Abort before it fires.
        handle.abort();

        // Give it a moment to ensure no message arrives.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!ctx.has_messages(&agent));
    }
    #[tokio::test]
    async fn cancel_all_aborts_pending_timeouts() {
        // Shutdown must cancel every pending timer: a timer left running
        // after shutdown fires into a dead inbox and can emit spurious
        // relation events against stopped members.
        let ctx = SocialContext::new(10);
        let agent = AgentId::new("alpha");
        ctx.register_agent(agent.clone(), "testing");

        let tm = TimeoutManager::new(ctx.clone());
        let handle = tm.set_timeout(
            agent.clone(),
            "should never fire",
            Duration::from_secs(60),
            Some("task-1".to_string()),
            None,
        );

        ctx.cancel_all_timeouts();

        // The aborted timer task must return promptly instead of sleeping
        // out its full 60s deadline.
        tokio::time::timeout(Duration::from_secs(2), async {
            let _ = handle.await;
        })
        .await
        .expect("cancel_all must abort the pending timer task");

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            ctx.drain_inbox(&agent).is_empty(),
            "no timeout message may be delivered after cancel_all"
        );
    }

    #[tokio::test]
    async fn fired_reminder_must_not_cancel_pending_delegate_timeout() {
        // Regression: a SelfReminder carrying a task_id used to cancel the
        // still-pending delegate timeout registered under the bare key when
        // the runner drained it (cancel_timeouts_for_messages matches on
        // task_id). The "double insurance" reminder thus killed the very
        // timeout it was backing up — no follow-up nudge, plus a phantom
        // SuccessfulCollaboration toward the silent peer.
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        let tm = TimeoutManager::new(ctx.clone());
        let _delegate_timer = tm.set_timeout(
            alice.clone(),
            "delegation timed out",
            Duration::from_secs(300),
            Some("task-0".to_string()),
            Some(bob.clone()),
        );
        // Reminder on the same correlation id, firing long before the
        // delegate timeout. Bob never replies.
        let _replaced = tm.set_self_reminder(
            alice.clone(),
            "check on bob",
            Duration::from_secs(10),
            Some("task-0".to_string()),
        );

        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(11)).await;
        ctx.wait_for_message(&alice).await;
        let msgs = ctx.drain_inbox(&alice);
        assert_eq!(msgs.len(), 1, "reminder must fire at 10s");
        assert_eq!(msgs[0].message_type, MessageType::SelfReminder);
        ctx.cancel_timeouts_for_messages(&alice, &msgs);

        tokio::time::advance(Duration::from_secs(301)).await;
        ctx.wait_for_message(&alice).await;
        let late = ctx.drain_inbox(&alice);
        assert_eq!(late.len(), 1, "delegate timeout must survive the reminder");
        assert_eq!(late[0].message_type, MessageType::Timeout);
    }
    #[tokio::test]
    async fn set_self_reminder_reports_replacement() {
        // B6: re-registering a pending reminder key must be reported so
        // callers (and the LLM) know the first reminder was replaced.
        let ctx = SocialContext::new(10);
        let agent = AgentId::new("carol");
        ctx.register_agent(agent.clone(), "testing");

        let tm = TimeoutManager::new(ctx.clone());
        let first = tm.set_self_reminder(
            agent.clone(),
            "first reminder",
            Duration::from_secs(50),
            Some("same-key".to_string()),
        );
        assert!(
            !first,
            "first registration on a fresh key must not report replacement"
        );
        let second = tm.set_self_reminder(
            agent.clone(),
            "second reminder",
            Duration::from_secs(50),
            Some("same-key".to_string()),
        );
        assert!(
            second,
            "second registration on the same key must report replacement"
        );

        // A fresh key reports no replacement.
        let fresh = tm.set_self_reminder(
            agent.clone(),
            "third reminder",
            Duration::from_secs(50),
            Some("other-key".to_string()),
        );
        assert!(!fresh, "fresh key must not report replacement");

        // Replacing with `task_id = None` registers nothing — fresh.
        let no_task = tm.set_self_reminder(agent.clone(), "fourth", Duration::from_secs(50), None);
        assert!(!no_task, "task_id-less reminder registers nothing");
    }
    #[tokio::test(start_paused = true)]
    async fn replacement_flag_is_fresh_after_reminder_fires() {
        // Once a reminder has fired (and deregistered its key), a new
        // reminder on the same key is fresh again — the flag reports
        // replacement of a *pending* reminder, not key history.
        let ctx = SocialContext::new(10);
        let agent = AgentId::new("dora");
        ctx.register_agent(agent.clone(), "testing");

        let tm = TimeoutManager::new(ctx.clone());
        let _first = tm.set_self_reminder(
            agent.clone(),
            "first reminder",
            Duration::from_secs(1),
            Some("key".to_string()),
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        ctx.wait_for_message(&agent).await;
        let fired = ctx.drain_inbox(&agent);
        assert_eq!(fired.len(), 1, "first reminder must have fired");

        let again = tm.set_self_reminder(
            agent.clone(),
            "second reminder",
            Duration::from_secs(1),
            Some("key".to_string()),
        );
        assert!(
            !again,
            "re-set after the previous reminder fired must be fresh"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn different_key_does_not_abort_other_timer() {
        // Replacement is per-key: registering a reminder under a different
        // key must leave the other key's timer pending (it still fires).
        let ctx = SocialContext::new(10);
        let agent = AgentId::new("eve");
        ctx.register_agent(agent.clone(), "testing");

        let tm = TimeoutManager::new(ctx.clone());
        let _a = tm.set_self_reminder(
            agent.clone(),
            "reminder a",
            Duration::from_secs(1),
            Some("key-a".to_string()),
        );
        let _b = tm.set_self_reminder(
            agent.clone(),
            "reminder b",
            Duration::from_secs(2),
            Some("key-b".to_string()),
        );

        tokio::time::advance(Duration::from_secs(1)).await;
        ctx.wait_for_message(&agent).await;
        let msgs = ctx.drain_inbox(&agent);
        assert_eq!(msgs.len(), 1, "key-a reminder must have fired");
        assert_eq!(msgs[0].body, "reminder a");

        tokio::time::advance(Duration::from_secs(1)).await;
        ctx.wait_for_message(&agent).await;
        let late = ctx.drain_inbox(&agent);
        assert_eq!(
            late.len(),
            1,
            "key-b reminder must survive key-a's registration"
        );
        assert_eq!(late[0].body, "reminder b");
    }
}
