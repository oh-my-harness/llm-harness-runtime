//! Self-driving agent loop: drives an [`AgentHarness`] from inbox messages.
//!
//! ## Architecture
//!
//! Two independent injection paths:
//!
//! 1. **Idle → Wake**: `AgentRunner` blocks on `wait_for_message` when idle.
//!    A message push triggers `Notify` → wakes → drains inbox → `harness.run()`.
//!
//! 2. **Running → Steer**: During `harness.run()`, a monitor task drains the
//!    inbox and calls `harness.steer()`. The loop drains steer at turn
//!    boundaries (`turn_index > 0`), injecting messages into `ctx.messages`.
//!    The monitor task is aborted when `run()` completes.
//!

use llm_harness_agent::AgentHarness;
use llm_harness_types::RunRequest;
use std::sync::Arc;
use tokio::task::{AbortHandle, JoinHandle};
use tracing::{debug, error, warn};

use crate::AgentId;
use crate::inbox::{InboxMessage, MessageType};
use crate::social_context::{AgentStatus, SocialContext};
use futures::FutureExt;
use llm_harness_agent::session::types::{SessionEntry, SessionEntryPayload};
use llm_harness_types::{AgentMessage, ContentBlock, UserMessage};

// ── format helper ────────────────────────────────────────────────────────────

/// Format drained inbox messages into a single user-facing string.
///
/// Messages carrying the same `broadcast_id` (volunteer replies to a
/// broadcast) are aggregated into a single list so the recipient can
/// compare candidates at a glance:
///
/// ```text
/// You have new messages:
///
/// [from: operator, type: Direct] do task X
/// [Volunteer responses to broadcast abc123 (2 replies)]
///   - agent-A: "I can help with this"
///   - agent-B: "I have experience here"
/// ```
pub fn format_inbox_messages(messages: &[InboxMessage]) -> String {
    use std::collections::BTreeMap;

    // Partition messages: only Direct replies carrying a broadcast_id
    // (volunteer replies) are grouped by broadcast_id; everything else —
    // including non-Direct messages that happen to carry a broadcast_id —
    // is rendered individually. BTreeMap keeps group order deterministic.
    let mut standalone: Vec<&InboxMessage> = Vec::new();
    let mut groups: BTreeMap<&str, Vec<&InboxMessage>> = BTreeMap::new();
    for msg in messages {
        if msg.message_type == MessageType::Direct
            && let Some(bid) = &msg.broadcast_id
        {
            groups.entry(bid.as_str()).or_default().push(msg);
            continue;
        }
        standalone.push(msg);
    }

    let mut out = String::from("You have new messages:\n\n");

    for msg in standalone {
        let correlation = match &msg.task_id {
            Some(task_id) => format!(", task: {task_id}"),
            None => String::new(),
        };
        out.push_str(&format!(
            "[from: {}, type: {}{correlation}] {}\n",
            msg.from,
            message_type_label(&msg.message_type),
            msg.body.replace('\n', "\n  ")
        ));
    }

    for (broadcast_id, replies) in groups {
        out.push_str(&format!(
            "[Volunteer responses to broadcast {} ({} repl{})]\n",
            broadcast_id,
            replies.len(),
            if replies.len() == 1 { "y" } else { "ies" }
        ));
        for msg in replies {
            out.push_str(&format!(
                "  - {}: \"{}\"\n",
                msg.from,
                msg.body.replace('\n', "\n  ")
            ));
        }
    }

    out
}

fn message_type_label(mt: &MessageType) -> &'static str {
    match mt {
        MessageType::Direct => "Direct",
        MessageType::DelegationRequest => "DelegationRequest",
        MessageType::Broadcast => "Broadcast",
        MessageType::Timeout => "Timeout",
        MessageType::SelfReminder => "SelfReminder",
        MessageType::ReflectionTrigger => "ReflectionTrigger",
    }
}

// ── ReflectionTrigger ────────────────────────────────────────────────────────

/// Buffers run transcripts and triggers a reflection task when enough runs
/// have accumulated.
///
/// After each `harness.run()`, the runner calls [`record_run`](Self::record_run)
/// with the session transcript. When the counter reaches `interval`, the
/// buffered transcripts are packed into a reflection task message and returned
/// for the caller to push into the agent's inbox. The counter and buffer then
/// reset.
pub struct ReflectionTrigger {
    interval: usize,
    count: usize,
    transcripts: Vec<String>,
    /// Number of session entries already consumed by previous `record_run`
    /// calls; the active-path transcript is append-only, so everything at or
    /// before this index was already included in an earlier prompt (T20).
    last_entry_count: usize,
}

impl ReflectionTrigger {
    /// Create a new trigger that fires after every `interval` runs.
    /// `interval == 0` disables reflection (the trigger never fires).
    pub fn new(interval: usize) -> Self {
        Self {
            interval,
            count: 0,
            transcripts: Vec::new(),
            last_entry_count: 0,
        }
    }

    /// Record a completed run. `entries` is the full active-path transcript
    /// from `read_active_path`; only entries past the internal cursor
    /// (`last_entry_count`) are formatted into this run's transcript segment,
    /// and the cursor advances to `entries.len()`. An empty slice still
    /// counts the run but records an empty transcript.
    ///
    /// Pass `None` when the transcript read failed: the run still counts
    /// toward the reflection cadence, but the cursor is left untouched so the
    /// next successful read covers everything since the last consumed entry.
    ///
    /// Returns `Some(message)` when the reflection threshold is reached,
    /// containing the assembled reflection task text. The caller should push
    /// this into the agent's inbox as a `MessageType::ReflectionTrigger`.
    pub fn record_run(&mut self, entries: Option<&[SessionEntry]>) -> Option<String> {
        if self.interval == 0 {
            return None;
        }
        if let Some(entries) = entries {
            let start = self.last_entry_count.min(entries.len());
            self.transcripts.push(format_transcript_capped(
                &entries[start..],
                REFLECTION_TRANSCRIPT_MAX_CHARS,
            ));
            self.last_entry_count = entries.len();
        }

        self.count += 1;
        if self.count >= self.interval {
            let task = build_reflection_task(&self.transcripts);
            self.count = 0;
            self.transcripts.clear();
            Some(task)
        } else {
            None
        }
    }

    /// The configured interval (0 = disabled).
    pub fn interval(&self) -> usize {
        self.interval
    }
}

// ── transcript helpers ───────────────────────────────────────────────────────

/// Format the session transcript into a text summary for the reflection prompt.
pub fn format_transcript(entries: &[SessionEntry]) -> String {
    let mut out = String::new();
    for entry in entries {
        match &entry.payload {
            SessionEntryPayload::Message(AgentMessage::User(msg)) => {
                for block in &msg.content {
                    if let ContentBlock::Text { text } = block {
                        out.push_str(&format!("[User] {text}\n"));
                    }
                }
            }
            SessionEntryPayload::Message(AgentMessage::Assistant(msg)) => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            out.push_str(&format!("[Assistant] {text}\n"));
                        }
                        ContentBlock::ToolUse { name, input, .. } => {
                            out.push_str(&format!("[Tool Call] {name}({input})\n"));
                        }
                        _ => {}
                    }
                }
            }
            SessionEntryPayload::Message(AgentMessage::ToolResult(msg)) => {
                let summary: String = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        llm_harness_types::DataBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                out.push_str(&format!("[Tool Result] {summary}\n"));
            }
            _ => {}
        }
    }
    out
}

/// Maximum number of characters of transcript content included in one
/// reflection prompt segment (T20).
pub(crate) const REFLECTION_TRANSCRIPT_MAX_CHARS: usize = 20_000;

/// Format entries into a transcript summary, capped at `max_chars` (T20).
///
/// Under the cap this is [`format_transcript`]. Over the cap, the output
/// keeps the HEAD (earliest context) plus the most recent entries, and
/// prepends a `"[... N earlier lines omitted ...]"` marker for the dropped
/// middle. Budgets are enforced in bytes (`str::len`), an upper bound on
/// character count, so the output never exceeds `max_chars` characters.
pub(crate) fn format_transcript_capped(entries: &[SessionEntry], max_chars: usize) -> String {
    let full = format_transcript(entries);
    if full.len() <= max_chars {
        return full;
    }
    let lines: Vec<&str> = full.lines().collect();
    if lines.is_empty() {
        return full;
    }

    // Reserve room for the omission marker (peaks around 42 bytes, e.g.
    // "[... 1234567 earlier lines omitted ...]"); split the remaining budget
    // evenly between earliest (head) and most recent (tail) lines.
    const MARKER_RESERVE: usize = 64;
    let budget = max_chars.saturating_sub(MARKER_RESERVE).max(1);
    let head_budget = budget / 2;
    let tail_budget = budget - head_budget;

    let mut head_len = 0;
    let mut head_end = 0;
    for (i, line) in lines.iter().enumerate() {
        let line_len = line.len() + 1; // newline
        if head_len + line_len > head_budget {
            break;
        }
        head_len += line_len;
        head_end = i + 1;
    }

    let mut tail_len = 0;
    let mut tail_start = lines.len();
    for line in lines[head_end..].iter().rev() {
        let line_len = line.len() + 1;
        if tail_len + line_len > tail_budget {
            break;
        }
        tail_len += line_len;
        tail_start -= 1;
        if tail_start == head_end {
            break;
        }
    }

    let omitted = tail_start - head_end;
    let mut out = String::with_capacity(max_chars);
    if omitted > 0 {
        out.push_str(&format!("[... {omitted} earlier lines omitted ...]\n"));
    }
    out.push_str(&lines[..head_end].join("\n"));
    out.push('\n');
    if tail_start == lines.len() && head_end < lines.len() {
        // Even the newest line exceeded the tail budget: slice it to the
        // remaining tail room instead of dropping the most recent content.
        let last = lines[lines.len() - 1];
        let mut take = (tail_budget - tail_len).min(last.len());
        // Byte budgets can land inside a multi-byte UTF-8 char; walk back to
        // a char boundary so the slice cannot panic.
        while take > 0 && !last.is_char_boundary(take) {
            take -= 1;
        }
        out.push_str(&last[..take]);
        out.push('\n');
    } else {
        out.push_str(&lines[tail_start..].join("\n"));
        out.push('\n');
    }
    out
}

/// Build the reflection task message pushed to the agent's inbox.
///
/// The agent first judges whether the accumulated transcripts contain a
/// REUSABLE lesson worth persisting; trivial or task-specific details
/// should NOT be written to memory (abstaining is correct). When it does
/// write, it must check for duplicates via `knowledge_search` first.
pub fn build_reflection_task(transcripts: &[String]) -> String {
    let mut out = String::from(
        "You have accumulated run history. Review the transcripts below.\n\n\
         First judge whether they contain anything worth persisting: a \
         REUSABLE lesson (what worked, what didn't, what to do differently) \
         that would apply to future tasks — not one-off details of these \
         specific runs.\n\n\
         If yes: use knowledge_search to check for existing similar lessons \
         (avoid duplicates), then persist the lesson via memory_write.\n\
         If no: skip memory_write entirely. Persisting trivial or \
         task-specific details pollutes your memory — abstaining is correct \
         when nothing clears the bar.\n\n",
    );
    for (i, transcript) in transcripts.iter().enumerate() {
        out.push_str(&format!("--- Run #{} ---\n{transcript}\n\n", i + 1));
    }
    out
}

// ── monitor task ─────────────────────────────────────────────────────────────

/// Spawn a monitor task that drains the agent's inbox and steers the harness
/// during a `harness.run()` call.
///
/// The monitor loop: `wait_for_message` → drain → `harness.steer(text)`.
/// It runs concurrently with `harness.run()` and is aborted when `run()`
/// completes.
///
/// The returned handle is a thin supervisor: the supervisor body registers
/// the monitor loop's abort handle in `ctx`'s monitor registry (no
/// spawn-to-registration window for shutdown to slip through), waits for
/// the loop, and removes the entry on normal exit. Aborting the
/// supervisor (the runner's `monitor.abort()`) cascades to the loop via a
/// drop guard armed before the first await. Shutdown aborts monitors
/// directly through the registry (see [`SocialContext::abort_monitors`]).
pub(crate) fn spawn_monitor_task(
    agent_id: AgentId,
    harness: Arc<AgentHarness>,
    ctx: SocialContext,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Spawn the real monitor loop and register its abort handle from
        // within the supervisor body.
        let loop_task = tokio::spawn(monitor_loop(agent_id.clone(), harness, ctx.clone()));

        // If the supervisor itself is aborted (the runner's
        // `monitor.abort()`) the loop must die with it: the guard is armed
        // right after spawn — before any await — so no abort window exists.
        struct ForwardAbort(Option<AbortHandle>);
        impl Drop for ForwardAbort {
            fn drop(&mut self) {
                if let Some(handle) = self.0.take() {
                    handle.abort();
                }
            }
        }
        let _forward = ForwardAbort(Some(loop_task.abort_handle()));
        ctx.register_monitor(&agent_id, loop_task.abort_handle());
        let _ = loop_task.await;
        ctx.unregister_monitor(&agent_id);
    })
}

/// The monitor loop proper: drain inbox → steer the harness while the
/// associated run is live; roll messages back when the steer channel is
/// unavailable.
async fn monitor_loop(agent_id: AgentId, harness: Arc<AgentHarness>, ctx: SocialContext) {
    loop {
        ctx.wait_for_message(&agent_id).await;

        let messages = ctx.drain_inbox(&agent_id);
        if messages.is_empty() {
            continue;
        }

        // Cancel pending timeouts whose key matches a drained reply
        // (task_id or broadcast_id) — the awaited response has arrived.
        // Peer-carrying (delegation) registrations only accept replies from
        // the registered peer; peer-less ones and broadcast deadlines accept
        // any sender.
        ctx.cancel_timeouts_for_messages(&agent_id, &messages);

        let text = format_inbox_messages(&messages);
        if let Err(e) = harness.steer_message(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text }],
            timestamp: chrono::Utc::now(),
        })) {
            // The run loop has stopped consuming (the main loop aborted
            // this monitor or the channel was reset) — the steer would
            // otherwise vanish together with the old channel. Roll the
            // messages back into the social inbox so the next run picks
            // them up. Cancellation of already-cancelled timeouts is a
            // no-op on re-delivery.
            warn!(
                agent = %agent_id,
                error = ?e,
                count = messages.len(),
                "steer channel unavailable, returning messages to inbox"
            );
            ctx.return_messages(&agent_id, messages);
            // Yield before retrying: the re-queued messages make
            // has_messages() true immediately, so without an await
            // point this branch hot-spins and even abort() can't reach
            // the task. 1ms bounds the retry rate without a counter.
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            continue;
        }
        debug!(
            agent = %agent_id,
            "steered inbox messages into harness"
        );
    }
}

// ── AgentRunner ──────────────────────────────────────────────────────────────

/// Owns a harness + social context and runs the self-driving loop.
///
/// The loop:
/// 1. Block on `wait_for_message` (idle).
/// 2. Mark Busy, drain inbox, build `RunRequest`.
/// 3. Spawn monitor task (inbox → steer during run).
/// 4. `harness.run(request)`.
/// 5. Abort monitor task.
/// 6. Record transcript; if reflection threshold reached, push reflection
///    task into inbox.
/// 7. If no more messages, mark Idle.
pub struct AgentRunner {
    agent_id: AgentId,
    harness: Arc<AgentHarness>,
    ctx: SocialContext,
    reflection: Option<ReflectionTrigger>,
    /// Knowledge access context injected into every run request (T15).
    knowledge_access: Option<Arc<llm_harness_knowledge::KnowledgeAccessContext>>,
}

impl AgentRunner {
    /// Assemble a runner from its components.
    ///
    /// Pass `Some(ReflectionTrigger::new(n))` to enable reflection after
    /// every `n` runs; `None` to disable. `knowledge_access` (T15) is
    /// injected into every run request so knowledge/memory tools resolve
    /// their `KnowledgeRequestContext`; `None` leaves requests untouched.
    pub fn new(
        agent_id: AgentId,
        harness: Arc<AgentHarness>,
        ctx: SocialContext,
        reflection: Option<ReflectionTrigger>,
        knowledge_access: Option<Arc<llm_harness_knowledge::KnowledgeAccessContext>>,
    ) -> Self {
        Self {
            agent_id,
            harness,
            ctx,
            reflection,
            knowledge_access,
        }
    }
    /// Spawn the self-driving loop as a tokio task and return its handle.
    pub fn spawn(self) -> JoinHandle<()> {
        let agent_id = self.agent_id.clone();
        let harness = self.harness.clone();
        let ctx = self.ctx.clone();
        let mut reflection = self.reflection;
        let knowledge_access = self.knowledge_access;

        tokio::spawn(async move {
            let mut consecutive_failures: u32 = 0;
            loop {
                // ── Idle: block until a message arrives ────────────────────
                ctx.wait_for_message(&agent_id).await;

                // ── Busy: drain + run ──────────────────────────────────────
                ctx.update_status(&agent_id, AgentStatus::Busy);

                let messages = ctx.drain_inbox(&agent_id);
                if messages.is_empty() {
                    // Raced: message was consumed by another drainer.
                    ctx.update_status(&agent_id, AgentStatus::Idle);
                    continue;
                }

                // Cancel pending timeouts whose key matches a drained reply
                // (task_id or broadcast_id) — the awaited response has
                // arrived. Peer-carrying (delegation) registrations only
                // accept replies from the registered peer; peer-less ones
                // and broadcast deadlines accept any sender.
                ctx.cancel_timeouts_for_messages(&agent_id, &messages);

                // A run triggered solely by a ReflectionTrigger message is a
                // reflection run: it must not count toward the next
                // reflection cadence, or reflection feeds itself forever
                // (each reflection run re-triggers reflection). Mixed
                // batches (reflection + real task in one drain) still
                // count — the agent did real work in that run.
                let is_reflection_run = messages
                    .iter()
                    .all(|m| m.message_type == crate::inbox::MessageType::ReflectionTrigger);
                let text = format_inbox_messages(&messages);
                let mut request = RunRequest::from_text(text);
                if let Some(access) = &knowledge_access {
                    // T15: knowledge/memory tools resolve their
                    // KnowledgeRequestContext from run extensions; without it
                    // every call fails with KnowledgeError::Unauthorized.
                    request = request.with_extension((**access).clone());
                }

                // Spawn monitor task: drain inbox → steer during run.
                let monitor = spawn_monitor_task(agent_id.clone(), harness.clone(), ctx.clone());

                // AssertUnwindSafe: the harness and ctx are Arc-backed shared
                // state that stays consistent across a panic (worse case: a
                // half-applied turn, which the next run overwrites). Without
                // this, one panicking run kills the runner task and leaves
                // the agent permanently Busy in the team directory.
                let run_result = std::panic::AssertUnwindSafe(harness.run(request))
                    .catch_unwind()
                    .await;

                // Abort the monitor task — run is over.
                monitor.abort();

                match run_result {
                    Ok(Ok(())) => {
                        // The run "completed" but the harness may have
                        // recorded an internal error mid-run
                        // (error_message — e.g. a provider failure surfaced
                        // as AgentEvent::Error, which drive_loop converts to
                        // Ok). Re-queue the consumed messages so they aren't
                        // silently lost; duplicate delivery beats loss.
                        // Aborts do not set error_message, so this never
                        // rolls back an aborted run.
                        if harness.state().error_message.is_some() {
                            consecutive_failures += 1;
                            error!(
                                runner = %agent_id,
                                error = ?harness.state().error_message,
                                "harness run failed after consuming messages; re-queuing"
                            );
                            ctx.return_messages(&agent_id, messages);
                        } else {
                            consecutive_failures = 0;
                            ctx.ack_inbox_messages(&messages);
                        }
                    }
                    Ok(Err(e)) => {
                        // Run failed: the drained messages were never
                        // processed. Return them to the inbox so the next
                        // run (or a retried run) handles them, instead of
                        // silently swallowing incoming work.
                        error!(agent = %agent_id, error = ?e, "harness run failed");
                        consecutive_failures += 1;
                        ctx.return_messages(&agent_id, messages);
                    }
                    Err(panic) => {
                        // Run panicked: same message-loss hazard as above,
                        // plus the status must not stay Busy.
                        let msg = panic
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".into());
                        error!(agent = %agent_id, panic = %msg, "harness run panicked");
                        consecutive_failures += 1;
                        ctx.return_messages(&agent_id, messages);
                    }
                }

                // ── Exponential backoff ───────────────────────────────────
                // Re-queueing makes has_messages() true immediately, so a
                // persistently failing run would hot-spin (fail → wake →
                // fail). After 3 consecutive failures, sleep
                // min(2^(n-3), 60) seconds before the retry; any success
                // resets the counter.
                if consecutive_failures >= 3 {
                    let shift = (consecutive_failures - 3).min(6);
                    let secs = (1u64 << shift).min(60);
                    warn!(
                        agent = %agent_id,
                        consecutive_failures,
                        backoff_secs = secs,
                        "harness run keeps failing; backing off before retry"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                }

                // ── Reflection trigger ─────────────────────────────────────
                // Buffer this run's transcript. When the threshold is reached,
                // push a reflection task into the agent's own inbox.
                // Reflection-only runs are skipped entirely: their transcript
                // is just the prior reflection, and counting them re-triggers
                // reflection forever.
                if let Some(trigger) = &mut reflection {
                    if is_reflection_run {
                        debug!(
                            agent = %agent_id,
                            "reflection-only run: not counting toward next reflection"
                        );
                    } else {
                        let entries = match harness.read_active_path().await {
                            Ok(entries) => Some(entries),
                            Err(e) => {
                                warn!(
                                    agent = %agent_id,
                                    error = ?e,
                                    "failed to read transcript for reflection"
                                );
                                None
                            }
                        };
                        if let Some(task_text) = trigger.record_run(entries.as_deref()) {
                            debug!(
                                agent = %agent_id,
                                "reflection threshold reached, pushing reflection task to inbox"
                            );
                            // push_message, not send_direct: a self-directed
                            // send would add the agent to its own contact list
                            // and advance its tick (accelerating real contacts'
                            // decay). Reflection is a system trigger, not a
                            // social interaction.
                            ctx.push_message(InboxMessage {
                                journal_id: None,
                                from: agent_id.clone(),
                                to: agent_id.clone(),
                                body: task_text,
                                message_type: MessageType::ReflectionTrigger,
                                priority: crate::inbox::priority::REFLECTION,
                                task_id: None,
                                broadcast_id: None,
                            });
                        }
                    }
                }

                // ── Back to idle if no more messages ───────────────────────
                if !ctx.has_messages(&agent_id) {
                    ctx.update_status(&agent_id, AgentStatus::Idle);
                }
            }
        })
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn format_inbox_messages_includes_type_labels() {
        let msgs = vec![
            InboxMessage {
                journal_id: None,
                from: AgentId::new("alpha"),
                to: AgentId::new("beta"),
                body: "hello".into(),
                message_type: MessageType::Direct,
                priority: 0,
                task_id: None,
                broadcast_id: None,
            },
            InboxMessage {
                journal_id: None,
                from: AgentId::new(crate::timeout_manager::reserved::TIMER),
                to: AgentId::new("beta"),
                body: "timeout!".into(),
                message_type: MessageType::Timeout,
                priority: 50,
                task_id: Some("t1".into()),
                broadcast_id: None,
            },
        ];
        let text = format_inbox_messages(&msgs);
        assert!(text.contains("Direct"));
        assert!(text.contains("Timeout"));
        assert!(text.contains("hello"));
        assert!(text.contains("timeout!"));
    }

    #[test]
    fn format_inbox_aggregates_broadcast_replies() {
        let msgs = vec![
            InboxMessage {
                journal_id: None,
                from: AgentId::new(crate::timeout_manager::reserved::OPERATOR),
                to: AgentId::new("alice"),
                body: "do task X".into(),
                message_type: MessageType::Direct,
                priority: 0,
                task_id: None,
                broadcast_id: None,
            },
            InboxMessage {
                journal_id: None,
                from: AgentId::new("bob"),
                to: AgentId::new("alice"),
                body: "I can help with this".into(),
                message_type: MessageType::Direct,
                priority: 0,
                task_id: None,
                broadcast_id: Some("bid-123".into()),
            },
            InboxMessage {
                journal_id: None,
                from: AgentId::new("carol"),
                to: AgentId::new("alice"),
                body: "I have experience here".into(),
                message_type: MessageType::Direct,
                priority: 0,
                task_id: None,
                broadcast_id: Some("bid-123".into()),
            },
        ];
        let text = format_inbox_messages(&msgs);

        // Standalone message rendered normally.
        assert!(text.contains("[from: operator, type: Direct] do task X"));

        // Volunteer replies aggregated into a list, not shown individually.
        assert!(text.contains("Volunteer responses to broadcast bid-123 (2 replies)"));
        assert!(text.contains("- bob: \"I can help with this\""));
        assert!(text.contains("- carol: \"I have experience here\""));
    }

    #[test]
    fn broadcast_grouping_requires_direct_type() {
        let msgs = vec![
            InboxMessage {
                journal_id: None,
                from: AgentId::new("eve"),
                to: AgentId::new("alice"),
                body: "I can help".into(),
                message_type: MessageType::Direct,
                priority: 0,
                task_id: None,
                broadcast_id: Some("bid-x".into()),
            },
            InboxMessage {
                journal_id: None,
                from: AgentId::new("dave"),
                to: AgentId::new("alice"),
                body: "please take this task".into(),
                message_type: MessageType::DelegationRequest,
                priority: 0,
                task_id: Some("t9".into()),
                broadcast_id: Some("bid-x".into()),
            },
        ];
        let text = format_inbox_messages(&msgs);

        // Only the Direct message joins the broadcast group.
        assert!(text.contains("Volunteer responses to broadcast bid-x (1 reply)"));
        assert!(text.contains("- eve: \"I can help\""));
        assert!(!text.contains("- dave:"));

        // The DelegationRequest renders as a standalone message.
        assert!(
            text.contains("[from: dave, type: DelegationRequest, task: t9] please take this task")
        );
    }

    #[test]
    fn grouping_order_deterministic() {
        let mut msgs = Vec::new();
        for bid in ["bid-c", "bid-a", "bid-b"] {
            msgs.push(InboxMessage {
                journal_id: None,
                from: AgentId::new(bid),
                to: AgentId::new("alice"),
                body: format!("reply for {bid}"),
                message_type: MessageType::Direct,
                priority: 0,
                task_id: None,
                broadcast_id: Some(bid.into()),
            });
        }
        // HashMap iteration is randomized per call; repeat to flush out any
        // nondeterministic ordering.
        for _ in 0..20 {
            let text = format_inbox_messages(&msgs);
            let a = text.find("broadcast bid-a").unwrap();
            let b = text.find("broadcast bid-b").unwrap();
            let c = text.find("broadcast bid-c").unwrap();
            assert!(
                a < b && b < c,
                "groups not in sorted broadcast_id order:\n{text}"
            );
        }
    }

    #[test]
    fn multiline_body_keeps_indentation() {
        let msgs = vec![
            InboxMessage {
                journal_id: None,
                from: AgentId::new("x"),
                to: AgentId::new("alice"),
                body: "line1\nline2".into(),
                message_type: MessageType::Direct,
                priority: 0,
                task_id: None,
                broadcast_id: None,
            },
            InboxMessage {
                journal_id: None,
                from: AgentId::new("y"),
                to: AgentId::new("alice"),
                body: "r1\nr2".into(),
                message_type: MessageType::Direct,
                priority: 0,
                task_id: None,
                broadcast_id: Some("bid-m".into()),
            },
        ];
        let text = format_inbox_messages(&msgs);

        // Standalone body continuation lines are indented by two spaces.
        assert!(text.contains("[from: x, type: Direct] line1\n  line2\n"));
        // Same for bodies inside a broadcast group.
        assert!(text.contains("- y: \"r1\n  r2\""));
    }

    #[test]
    fn reflection_trigger_fires_after_n_runs() {
        let mut trigger = ReflectionTrigger::new(3);
        let mut entries: Vec<SessionEntry> = Vec::new();

        // Run 1 — not enough.
        entries.push(user_entry("transcript 1"));
        assert!(trigger.record_run(Some(&entries)).is_none());
        // Run 2 — not enough.
        entries.push(user_entry("transcript 2"));
        assert!(trigger.record_run(Some(&entries)).is_none());
        // Run 3 — threshold reached.
        entries.push(user_entry("transcript 3"));
        let task = trigger.record_run(Some(&entries));
        assert!(task.is_some());
        let task = task.unwrap();
        assert!(task.contains("Run #1"));
        assert!(task.contains("transcript 1"));
        assert!(task.contains("Run #2"));
        assert!(task.contains("transcript 2"));
        assert!(task.contains("Run #3"));
        assert!(task.contains("transcript 3"));
        assert!(task.contains("memory_write"));

        // Counter resets after firing.
        entries.push(user_entry("transcript 4"));
        assert!(trigger.record_run(Some(&entries)).is_none());
    }

    #[test]
    fn reflection_trigger_disabled_when_interval_zero() {
        let mut trigger = ReflectionTrigger::new(0);
        let entries = [user_entry("t1")];
        assert!(trigger.record_run(Some(&entries)).is_none());
        assert!(trigger.record_run(Some(&entries)).is_none());
    }

    /// Build a minimal user-message entry, mirroring the helper in
    /// llm-harness-agent's compaction tests.
    fn user_entry(text: &str) -> SessionEntry {
        SessionEntry {
            id: llm_harness_types::EntryId::new(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            payload: SessionEntryPayload::Message(AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text { text: text.into() }],
                timestamp: chrono::Utc::now(),
            })),
            compaction_policy: None,
        }
    }

    #[test]
    fn reflection_reads_only_new_content() {
        let mut trigger = ReflectionTrigger::new(1);
        let mut entries = vec![user_entry("first segment")];
        // First fire — includes the first segment.
        let p1 = trigger.record_run(Some(&entries)).unwrap();
        assert!(p1.contains("first segment"));

        // Append new content; the second fire must cover only the new entries.
        entries.push(user_entry("second segment"));
        let p2 = trigger.record_run(Some(&entries)).unwrap();
        assert!(p2.contains("second segment"));
        assert!(
            !p2.contains("first segment"),
            "second prompt re-included already-read content:\n{p2}"
        );
    }

    #[test]
    fn reflection_prompt_capped_at_20k_chars() {
        let mut entries = vec![user_entry("earliest context line")];
        for _ in 0..1500 {
            entries.push(user_entry(
                "filler line with enough padding to push the transcript past the cap",
            ));
        }
        entries.push(user_entry("most recent line"));

        let prompt = format_transcript_capped(&entries, REFLECTION_TRANSCRIPT_MAX_CHARS);
        assert!(
            prompt.len() <= REFLECTION_TRANSCRIPT_MAX_CHARS + 100,
            "prompt exceeds cap + marker overhead: {} bytes",
            prompt.len()
        );
        assert!(prompt.contains("earlier lines omitted"));
        assert!(
            prompt.contains("earliest context line"),
            "earliest context must be retained"
        );
        assert!(prompt.contains("most recent line"));
    }

    #[test]
    fn read_failure_does_not_rewind_cursor() {
        let mut trigger = ReflectionTrigger::new(1);
        let entries = vec![user_entry("first segment")];

        // First fire consumes the first segment.
        assert!(
            trigger
                .record_run(Some(&entries))
                .unwrap()
                .contains("first segment")
        );

        // Read failure: run counts, but the cursor must NOT rewind to 0 —
        // the next fire must not re-include the first segment.
        assert!(trigger.record_run(None).is_some());
        // The next successful read returns the full path: everything before
        // plus new content. The cursor (still 1) must slice past the old
        // entries and include only the appended one.
        let after = vec![user_entry("first segment"), user_entry("third segment")];
        let p3 = trigger.record_run(Some(&after)).unwrap();
        assert!(p3.contains("third segment"));
        assert!(
            !p3.contains("first segment"),
            "cursor rewound after a read failure; transcript re-read:\n{p3}"
        );
    }

    #[test]
    fn oversized_last_line_is_sliced_not_dropped() {
        // One line far larger than the tail budget: the most recent content
        // must survive (sliced), not vanish.
        let entries = vec![
            user_entry("filler line with enough padding to push the transcript past the cap"),
            user_entry(&"x".repeat(5000)),
        ];
        // Use a small cap so the tail budget (about 490 bytes) is smaller
        // than the 5,000-byte final line.
        let prompt = format_transcript_capped(&entries, 1_000);
        assert!(prompt.contains("earlier lines omitted"));
        assert!(
            prompt.contains("[User] xxxxx"),
            "oversized last line was dropped instead of sliced:\n{prompt}"
        );
    }

    #[test]
    fn oversized_multibyte_line_slices_on_char_boundary() {
        // 3-byte CJK chars: a byte-offset slice landing mid-char must not
        // panic (the runner calls this outside catch_unwind).
        let entries = vec![
            user_entry("filler line with enough padding to push the transcript past the cap"),
            user_entry(&"整".repeat(2000)),
        ];
        let prompt = format_transcript_capped(&entries, 1_000);
        assert!(prompt.contains("earlier lines omitted"));
        assert!(
            prompt.contains("[User] 整"),
            "multibyte last line was dropped instead of sliced:\n{prompt}"
        );
    }

    #[test]
    fn build_reflection_task_includes_all_transcripts() {
        let transcripts = vec![
            "run A transcript".to_string(),
            "run B transcript".to_string(),
        ];
        let task = build_reflection_task(&transcripts);
        assert!(task.contains("Run #1"));
        assert!(task.contains("run A transcript"));
        assert!(task.contains("Run #2"));
        assert!(task.contains("run B transcript"));
        assert!(task.contains("memory_write"));
        assert!(task.contains("knowledge_search"));
    }

    #[test]
    fn build_reflection_task_empty_transcripts() {
        let task = build_reflection_task(&[]);
        assert!(task.contains("memory_write"));
        assert!(!task.contains("Run #"));
    }

    // ── Concurrency: monitor steer rollback ─────────────────────────────

    /// Build a harness backed by NoOpLlmClient (never called in these
    /// tests — we only exercise the monitor task and channel semantics).
    async fn noop_harness() -> Arc<AgentHarness> {
        Arc::new(
            AgentHarness::new_in_memory(
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                llm_harness_agent::AgentHarnessOptions::new("test-model"),
            )
            .await,
        )
    }

    #[tokio::test]
    async fn monitor_returns_messages_to_inbox_when_steer_fails() {
        // Regression for the monitor race: steering into a harness whose
        // run loop isn't consuming fails (the initial steer channel's
        // receiver is dropped until run() creates a fresh pair). The
        // monitor must roll the drained messages back into the social
        // inbox instead of vanishing — previously harness.steer() ignored
        // the error and the messages were lost on both layers.
        let harness = noop_harness().await;
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        // Sanity: steering an idle harness fails (channel closed).
        assert!(
            harness
                .steer_message(RunRequest::from_text("x").initial_messages.remove(0))
                .is_err()
        );

        // Deliver a message and start the monitor WITHOUT ever running the
        // harness — the monitor drains, fails to steer, and must roll back.
        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "hello alice".into(),
            message_type: MessageType::Direct,
            priority: 0,
            task_id: None,
            broadcast_id: None,
        });
        let monitor = spawn_monitor_task(alice.clone(), harness.clone(), ctx.clone());
        // Current-thread runtime: give the spawned monitor its first poll
        // before asserting on observable state.
        tokio::task::yield_now().await;

        // Wait until the message is back in the inbox (monitor drained it,
        // steer failed, rollback pushed it back).
        for _ in 0..100 {
            if ctx.has_messages(&alice) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(ctx.has_messages(&alice), "message must roll back to inbox");
        let back = ctx.drain_inbox(&alice);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].body, "hello alice");
        assert_eq!(back[0].from, AgentId::new("bob"));

        monitor.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn monitor_task_yields_on_steer_failure_so_abort_works() {
        // Regression for the monitor hot-spin: when steer_message fails
        // (idle harness, steer channel closed), the drained messages are
        // returned to the inbox, so has_messages() is immediately true
        // again and the loop re-drains synchronously — without an await
        // point that yields, the task spins hot and even abort() can never
        // take effect. The failure branch must sleep, giving the scheduler
        // a chance to cancel the task.
        let harness = noop_harness().await;
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "hello alice".into(),
            message_type: MessageType::Direct,
            priority: 0,
            task_id: None,
            broadcast_id: None,
        });
        let monitor = spawn_monitor_task(alice.clone(), harness.clone(), ctx.clone());

        // Pre-fix: the task never yields → abort() never lands → join
        // hangs. Post-fix: the 1ms sleep in the failure branch lets abort
        // land.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        monitor.abort();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            // Aborted task: join returns Err(Cancelled) — that's fine, we
            // just need it to RETURN.
            let _ = monitor.await;
        })
        .await
        .expect("monitor task must be abortable while in the steer-failure branch");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_monitors_kills_registered_monitor() {
        // Regression for the shutdown leak: a monitor whose runner is
        // aborted mid-run is never reaped by the runner's own
        // `monitor.abort()` — the handle lives only in the runner's frame.
        // The ctx must track spawned monitors so shutdown can kill them.
        let harness = noop_harness().await;
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "hello alice".into(),
            message_type: MessageType::Direct,
            priority: 0,
            task_id: None,
            broadcast_id: None,
        });
        let monitor = spawn_monitor_task(alice.clone(), harness.clone(), ctx.clone());

        // The spawned monitor must end up registered in the ctx registry —
        // wait for the supervisor to get its first poll.
        let mut registered = false;
        for _ in 0..100 {
            if ctx.monitor_count() == 1 {
                registered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            registered,
            "spawned monitor must be registered in the ctx monitor registry"
        );
        // Let the monitor start draining, then kill it via the registry —
        // the spawner's own handle is not consulted.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        ctx.abort_monitors();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let _ = monitor.await;
        })
        .await
        .expect("abort_monitors must kill the registered monitor task");
        assert_eq!(
            ctx.monitor_count(),
            0,
            "abort_monitors must clear the monitor registry"
        );
    }

    #[tokio::test]
    async fn reflection_message_does_not_self_contact() {
        // Regression: reflection used send_direct(self, self), which added
        // the agent to its own contact list. Now it pushes a
        // ReflectionTrigger message with no contact side effects.
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        ctx.push_message(InboxMessage {
            journal_id: None,
            from: alice.clone(),
            to: alice.clone(),
            body: "reflect".into(),
            message_type: MessageType::ReflectionTrigger,
            priority: crate::inbox::priority::REFLECTION,
            task_id: None,
            broadcast_id: None,
        });

        assert!(ctx.has_messages(&alice));
        assert!(
            !ctx.has_contact(&alice, &alice),
            "self-message must not create a self-contact"
        );
        let msgs = ctx.drain_inbox(&alice);
        assert_eq!(msgs[0].message_type, MessageType::ReflectionTrigger);
    }

    #[tokio::test]
    async fn run_failure_returns_messages_to_inbox() {
        // Regression: a failed harness.run() used to swallow the drained
        // messages. The runner must roll them back so the next run (or a
        // retried run) handles them, and must not leave the agent Busy.
        //
        // Failure injection: a BeforeRunHook returning an AgentError makes
        // run() fail before any LLM call (provider errors are converted to
        // loop events, not Err returns, by the drive loop). The hook fails
        // only the FIRST run — a permanent failure would spin the runner
        // forever (rollback → wake → fail → …). The SECOND run consuming
        // the rolled-back message then proves end-to-end redelivery.
        let hook_runs = Arc::new(std::sync::atomic::AtomicU32::new(0));
        struct FailingHook(Arc<std::sync::atomic::AtomicU32>);

        #[async_trait::async_trait]
        impl llm_harness_types::BeforeRunHook for FailingHook {
            fn before_run<'a>(
                &'a self,
                _ctx: llm_harness_types::BeforeRunCtx<'a>,
            ) -> futures::future::BoxFuture<
                'a,
                Result<llm_harness_types::BeforeRunResult, llm_harness_types::AgentError>,
            > {
                let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    if n == 0 {
                        Err(llm_harness_types::AgentError::Internal("hook boom".into()))
                    } else {
                        Ok(llm_harness_types::BeforeRunResult {
                            additional_messages: vec![],
                            system_prompt: None,
                        })
                    }
                })
            }
        }

        let mut opts = llm_harness_agent::AgentHarnessOptions::new("test-model");
        let hook = Arc::new(FailingHook(hook_runs.clone()));
        opts.hooks.before_run.push(hook);
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                opts,
            )
            .await,
        );
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        // Deliver work, then let the runner hit the failing run. The direct
        // pre-check that the hook fails a run is implicit: run #0 fails, so
        // the message rolls back; run #2 (hook ok) consumes it.
        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "work to do".into(),
            message_type: MessageType::Direct,
            priority: 0,
            task_id: None,
            broadcast_id: None,
        });

        let runner = AgentRunner::new(alice.clone(), harness.clone(), ctx.clone(), None, None);
        let task = runner.spawn();

        // End-state assertions (the drain→fail→rollback→retry sequence
        // completes between 10ms polls, so intermediate inbox windows are
        // unobservable — the stable evidence is the end state plus the
        // hook's call count):
        // 1. The agent settles Idle with an empty inbox — the rolled-back
        //    message was consumed by the retry run.
        // 2. The hook ran at least twice: the failed run AND the retry.
        //    (A swallowed message would leave the hook at 1 forever.)
        for _ in 0..200 {
            if ctx.get_agent_info(&alice).map(|i| i.status) == Some(AgentStatus::Idle)
                && !ctx.has_messages(&alice)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!task.is_finished(), "runner must survive a failed run");
        assert_eq!(
            ctx.get_agent_info(&alice).unwrap().status,
            AgentStatus::Idle,
            "failed run must not leave the agent Busy"
        );
        assert!(
            !ctx.has_messages(&alice),
            "retry run must consume the rolled-back message"
        );
        assert!(
            hook_runs.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "hook must have observed the failed run and the retry run"
        );

        task.abort();
    }

    #[tokio::test]
    async fn completed_run_with_internal_error_returns_messages_to_inbox() {
        // Regression: when a harness run "completes" but the harness
        // recorded an internal error mid-run (error_message set — e.g. a
        // provider failure surfaced as AgentEvent::Error), drive_loop
        // returns Ok and the Ok(Ok(())) arm treated it as success,
        // silently losing the drained messages. The runner must re-queue
        // them (duplicate delivery beats silent loss).
        //
        // Failure injection: MockLlmClient with a rate-limit error preset
        // first, then a success. Default options disable retries
        // (retry: None → max_retries 0), so chat_stream Err(RateLimit)
        // yields AgentEvent::Error → the harness sets error_message and
        // run() returns Ok(()) — exactly the Ok(Ok(())) arm. A permanent
        // failure would hot-spin the runner (re-queue → wake → fail → …),
        // so the SECOND call succeeds: the retry run consuming the
        // re-queued message proves redelivery end-to-end. The inbox
        // window between re-queue and re-drain is unobservable (the
        // runner re-drains immediately), so the retry's LLM call is the
        // stable evidence.
        let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
            llm_harness_loop::test_utils::MockResponse::rate_limit_error(),
            llm_harness_loop::test_utils::MockResponse::text("recovered"),
        ]));
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                client.clone() as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                llm_harness_agent::AgentHarnessOptions::new("test-model"),
            )
            .await,
        );
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "work to do".into(),
            message_type: MessageType::Direct,
            priority: 0,
            task_id: None,
            broadcast_id: None,
        });

        let runner = AgentRunner::new(alice.clone(), harness.clone(), ctx.clone(), None, None);
        let task = runner.spawn();

        // End-state assertions: post-fix, the errored run re-queues the
        // message and the retry run consumes it (agent Idle, inbox empty,
        // two LLM calls). Pre-fix, the runner goes Idle right after the
        // failed run with the message silently lost (one LLM call, never
        // retried).
        for _ in 0..200 {
            if ctx.get_agent_info(&alice).map(|i| i.status) == Some(AgentStatus::Idle)
                && !ctx.has_messages(&alice)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            ctx.get_agent_info(&alice).unwrap().status,
            AgentStatus::Idle,
            "completed-with-error run must not leave the agent Busy"
        );
        assert!(
            !ctx.has_messages(&alice),
            "retry run must consume the re-queued message"
        );
        assert!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "the errored run's message must be re-queued and re-consumed by \
             a retry run (pre-fix it is silently lost: call_count stays 1)"
        );

        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn build_retry_backs_off_exponentially() {
        // Regression (T3): a permanently failing run re-queues its messages
        // (T2), which makes has_messages() true immediately — the loop
        // re-drains and re-fails in a hot spin. After 3 consecutive failures
        // the runner must back off exponentially (1s, 2s, 4s, … cap 60s) and
        // reset the counter on any success.
        //
        // Injection: a BeforeRunHook that fails BEFORE any LLM call, so every
        // attempt is a clean Ok(Err(..)) failure. The hook's outcome is
        // snapshotted at CALL time (not resume time) so flipping `succeed`
        // mid-test never leaks into an in-flight attempt, and it yields once
        // (a Pending point) so attempts interleave deterministically with the
        // test's time advances under paused time.
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let succeed = Arc::new(AtomicBool::new(false));
        struct CountingFailHook(Arc<std::sync::atomic::AtomicUsize>, Arc<AtomicBool>);

        #[async_trait::async_trait]
        impl llm_harness_types::BeforeRunHook for CountingFailHook {
            fn before_run<'a>(
                &'a self,
                _ctx: llm_harness_types::BeforeRunCtx<'a>,
            ) -> futures::future::BoxFuture<
                'a,
                Result<llm_harness_types::BeforeRunResult, llm_harness_types::AgentError>,
            > {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let fail = !self.1.load(std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    if fail {
                        Err(llm_harness_types::AgentError::Internal("hook boom".into()))
                    } else {
                        Ok(llm_harness_types::BeforeRunResult {
                            additional_messages: vec![],
                            system_prompt: None,
                        })
                    }
                })
            }
        }

        let mut opts = llm_harness_agent::AgentHarnessOptions::new("test-model");
        opts.hooks.before_run.push(Arc::new(CountingFailHook(
            attempts.clone(),
            succeed.clone(),
        )));
        // One success response, consumed by the reset-proof attempt below.
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
                    llm_harness_loop::test_utils::MockResponse::text("done"),
                ])) as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                opts,
            )
            .await,
        );
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "work to do".into(),
            message_type: MessageType::Direct,
            priority: 0,
            task_id: None,
            broadcast_id: None,
        });

        let runner = AgentRunner::new(alice.clone(), harness.clone(), ctx.clone(), None, None);
        let task = runner.spawn();

        // Phase 1: attempts 1–3 must run with NO backoff (backoff only
        // starts after the 3rd consecutive failure). The exit condition is
        // the post-3rd-failure re-queue (attempt 3 counted AND the message
        // back in the inbox) — at that instant the runner has scheduled the
        // 1s backoff sleep, which phases 2–3 pin. 5s budget absorbs
        // scheduling noise.
        let mut elapsed = std::time::Duration::ZERO;
        while attempts.load(std::sync::atomic::Ordering::SeqCst) < 3 || !ctx.has_messages(&alice) {
            assert!(
                elapsed <= std::time::Duration::from_millis(5_000),
                "first 3 attempts must not back off; stalled at {elapsed:?}"
            );
            tokio::time::advance(std::time::Duration::from_millis(10)).await;
            tokio::task::yield_now().await;
            elapsed += std::time::Duration::from_millis(10);
        }

        // Phase 2: pin the 1s lower bound EXACTLY. The runner is parked on
        // the freshly-scheduled 1s sleep; the clock is frozen outside
        // advance(), so advancing 999ms must NOT fire the 4th attempt.
        tokio::time::advance(std::time::Duration::from_millis(999)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "attempt 4 must not fire before the full 1s backoff has elapsed"
        );

        // Phase 3: pin the upper edge — the 4th attempt fires at ~1.0s
        // (1s sleep + scheduling margin), never later than 1.1s.
        let mut since_bound = std::time::Duration::ZERO;
        while attempts.load(std::sync::atomic::Ordering::SeqCst) < 4 {
            assert!(
                since_bound <= std::time::Duration::from_millis(100),
                "attempt 4 must fire promptly after the 1s backoff; \
                 took {since_bound:?} extra",
            );
            tokio::time::advance(std::time::Duration::from_millis(10)).await;
            tokio::task::yield_now().await;
            since_bound += std::time::Duration::from_millis(10);
        }

        // Phase 4: reset-on-success. Flip the hook to succeed — attempt 4
        // still fails (its outcome was snapshotted at call time), schedules
        // a 2s backoff (n=4), and the succeeding attempt 5 then resets the
        // counter. The 3s budget absorbs that outstanding 2s sleep. An
        // UNRESET counter would surface in the next segment instead.
        succeed.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut since_success_flip = std::time::Duration::ZERO;
        while attempts.load(std::sync::atomic::Ordering::SeqCst) < 5 {
            assert!(
                since_success_flip <= std::time::Duration::from_millis(3_000),
                "succeeding attempt must fire after the outstanding backoff; \
                 stalled at {since_success_flip:?}"
            );
            tokio::time::advance(std::time::Duration::from_millis(10)).await;
            tokio::task::yield_now().await;
            since_success_flip += std::time::Duration::from_millis(10);
        }

        // Phase 5: the success consumed the inbox (no re-queue on success),
        // so push a fresh message, flip back to failing, and require two
        // prompt failures. This is the actual reset proof: the counter was
        // reset by attempt 5's success, so attempts 6–7 (n=1, 2) get NO
        // backoff; an unreset counter (n=5) would sleep 8s and trip the
        // 500ms budget.
        succeed.store(false, std::sync::atomic::Ordering::SeqCst);
        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "work again".into(),
            message_type: MessageType::Direct,
            priority: 0,
            task_id: None,
            broadcast_id: None,
        });
        let mut since_reset = std::time::Duration::ZERO;
        while attempts.load(std::sync::atomic::Ordering::SeqCst) < 7 {
            assert!(
                since_reset <= std::time::Duration::from_millis(500),
                "failures after a success must restart backoff from zero; \
                 stalled at {since_reset:?}"
            );
            tokio::time::advance(std::time::Duration::from_millis(10)).await;
            tokio::task::yield_now().await;
            since_reset += std::time::Duration::from_millis(10);
        }

        task.abort();
    }

    #[tokio::test]
    async fn reflection_threshold_pushes_reflection_trigger_message() {
        // Integration of runner + ReflectionTrigger: after `interval`
        // successful runs, the runner pushes a ReflectionTrigger message
        // into the agent's own inbox. The runner then consumes it as the
        // next run's input — observed here via the session transcript
        // (a stable, race-free observation point; an external drain would
        // race the runner's own drain).
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                llm_harness_agent::AgentHarnessOptions::new("test-model"),
            )
            .await,
        );
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        let runner = AgentRunner::new(
            alice.clone(),
            harness.clone(),
            ctx.clone(),
            Some(ReflectionTrigger::new(2)),
            None,
        );
        let task = runner.spawn();

        // Run 1: message delivered and consumed; reflection not yet fired.
        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "first job".into(),
            message_type: MessageType::Direct,
            priority: 0,
            task_id: None,
            broadcast_id: None,
        });
        for _ in 0..100 {
            if !ctx.has_messages(&alice) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            !ctx.has_messages(&alice),
            "first run must consume the message without firing reflection"
        );

        // Run 2 crosses the threshold: the runner pushes a ReflectionTrigger
        // message and then consumes it as run 3. Wait for the queue to go
        // empty AND the agent to settle Idle — the reflection run has then
        // fully completed.
        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "second job".into(),
            message_type: MessageType::Direct,
            priority: 0,
            task_id: None,
            broadcast_id: None,
        });
        for _ in 0..200 {
            if !ctx.has_messages(&alice)
                && ctx.get_agent_info(&alice).map(|i| i.status) == Some(AgentStatus::Idle)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            !ctx.has_messages(&alice)
                && ctx.get_agent_info(&alice).unwrap().status == AgentStatus::Idle,
            "reflection run must have completed (queue drained, agent idle)"
        );

        // Run 3's input must contain the reflection task text — proving the
        // ReflectionTrigger message was pushed and consumed.
        let entries = harness.read_active_path().await.unwrap();
        let all_text: String = entries
            .iter()
            .filter_map(|e| match &e.payload {
                SessionEntryPayload::Message(AgentMessage::User(u)) => Some(
                    u.content
                        .iter()
                        .filter_map(|c| match c {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n---\n");
        assert!(
            all_text.contains("memory_write"),
            "reflection task (memory_write prompt) must have been injected as a run input; got:\n{all_text}"
        );

        task.abort();
    }

    /// Provider that panics on its FIRST chat request, then delegates to a
    /// mock for subsequent calls. One-shot: a permanent panic would spin
    /// the runner forever (rollback → wake → panic → …); the post-panic
    /// success proves the runner survived and redelivered the message.
    struct PanicOnceProvider {
        panicked: std::sync::atomic::AtomicBool,
        inner: llm_harness_loop::test_utils::MockLlmClient,
    }

    #[async_trait::async_trait]
    impl llm_harness_loop::LlmClient for PanicOnceProvider {
        fn capabilities(&self) -> llm_harness_loop::ProviderCapabilities {
            self.inner.capabilities()
        }

        async fn chat(
            &self,
            req: &llm_harness_loop::ChatRequest,
        ) -> Result<llm_harness_loop::ChatResponse, llm_harness_loop::LlmError> {
            self.inner.chat(req).await
        }

        async fn chat_stream(
            &self,
            req: &llm_harness_loop::ChatRequest,
        ) -> Result<llm_harness_loop::StreamHandle, llm_harness_loop::LlmError> {
            if !self
                .panicked
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                panic!("provider exploded");
            }
            self.inner.chat_stream(req).await
        }
    }

    #[tokio::test]
    async fn reflection_only_runs_do_not_retrigger_reflection() {
        // Regression for the reflection self-recursion bug: with
        // interval=1, the reflection run itself used to count toward the
        // next trigger, producing an endless reflection→reflection loop
        // (observed live: "reflection triggers have looped ~6 times").
        // After the fix, a run triggered solely by a ReflectionTrigger
        // message is not counted — after the loop settles, exactly one
        // reflection task was produced and no further ones follow.
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                llm_harness_agent::AgentHarnessOptions::new("test-model"),
            )
            .await,
        );
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        let runner = AgentRunner::new(
            alice.clone(),
            harness.clone(),
            ctx.clone(),
            Some(ReflectionTrigger::new(1)),
            None,
        );
        let task = runner.spawn();

        // One real task run fires reflection (interval=1). The runner then
        // consumes the ReflectionTrigger as a reflection-only run — which
        // must NOT re-fire. Wait for the queue to drain and the agent to
        // settle Idle, then observe over a settle window: no further runs
        // may occur (an endless loop would keep the transcript growing and
        // the agent perpetually Busy).
        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "real job".into(),
            message_type: MessageType::Direct,
            priority: 0,
            task_id: None,
            broadcast_id: None,
        });
        for _ in 0..200 {
            if !ctx.has_messages(&alice)
                && ctx.get_agent_info(&alice).map(|i| i.status) == Some(AgentStatus::Idle)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            !ctx.has_messages(&alice)
                && ctx.get_agent_info(&alice).unwrap().status == AgentStatus::Idle,
            "reflection run must have completed (queue drained, agent idle)"
        );

        // Count reflection-task inputs in the session transcript: exactly
        // one. A recursion bug would keep adding more (each reflection run
        // embedding the last).
        async fn count_reflections(h: &AgentHarness) -> usize {
            h.read_active_path()
                .await
                .unwrap()
                .iter()
                .filter(|e| match &e.payload {
                    SessionEntryPayload::Message(AgentMessage::User(u)) => {
                        u.content.iter().any(|c| match c {
                            ContentBlock::Text { text } => {
                                text.contains("You have accumulated run history")
                            }
                            _ => false,
                        })
                    }
                    _ => false,
                })
                .count()
        }
        let after_settle = count_reflections(&harness).await;
        assert_eq!(
            after_settle, 1,
            "exactly one reflection task must be injected; recursion would add more"
        );

        // Give any (buggy) re-trigger a window to fire: the count must stay
        // at one and the agent must stay Idle.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(
            count_reflections(&harness).await,
            after_settle,
            "no further reflection tasks may be injected after the reflection-only run"
        );
        assert_eq!(
            ctx.get_agent_info(&alice).unwrap().status,
            AgentStatus::Idle,
            "agent must settle Idle — a recursion would keep it cycling Busy"
        );

        task.abort();
    }

    #[tokio::test]
    async fn run_panic_returns_messages_and_recovers_to_idle() {
        // Regression: a panicking harness.run() used to kill the runner
        // task, permanently stranding the agent in Busy with its drained
        // messages lost. catch_unwind must contain the panic: the messages
        // roll back and the loop continues to the next idle wait. The
        // post-panic retry run (provider no longer panicking) consuming
        // the rolled-back message proves end-to-end redelivery.
        let provider = Arc::new(PanicOnceProvider {
            panicked: std::sync::atomic::AtomicBool::new(false),
            inner: llm_harness_loop::test_utils::MockLlmClient::new(vec![]),
        });
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                provider.clone() as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                llm_harness_agent::AgentHarnessOptions::new("test-model"),
            )
            .await,
        );
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "work to do".into(),
            message_type: MessageType::Direct,
            priority: 0,
            task_id: None,
            broadcast_id: None,
        });

        let runner = AgentRunner::new(alice.clone(), harness.clone(), ctx.clone(), None, None);
        let task = runner.spawn();

        // End-state assertions (the drain→panic→rollback→retry sequence
        // completes between 10ms polls, so intermediate inbox windows are
        // unobservable — the stable evidence is the end state plus the
        // session transcript):
        // 1. The runner survived the panic.
        // 2. The agent settles Idle (a dead runner would strand Busy).
        // 3. The session shows TWO runs: the panicked one whose input
        //    ("work to do") was persisted, and the retry that consumed the
        //    rolled-back message.
        for _ in 0..200 {
            if ctx.get_agent_info(&alice).map(|i| i.status) == Some(AgentStatus::Idle)
                && !ctx.has_messages(&alice)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!task.is_finished(), "runner must survive a panicking run");
        assert_eq!(
            ctx.get_agent_info(&alice).unwrap().status,
            AgentStatus::Idle,
            "panicking run must not leave the agent Busy"
        );
        assert!(
            !ctx.has_messages(&alice),
            "retry run must consume the rolled-back message"
        );
        // Both runs received the same message text: the panicked first run
        // AND the retry (only possible if the drained message rolled back).
        let entries = harness.read_active_path().await.unwrap();
        let run_inputs: Vec<String> = entries
            .iter()
            .filter_map(|e| match &e.payload {
                SessionEntryPayload::Message(AgentMessage::User(u)) => Some(
                    u.content
                        .iter()
                        .filter_map(|c| match c {
                            ContentBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .filter(|t| t.contains("work to do"))
            .collect();
        assert!(
            run_inputs.len() >= 2,
            "the rolled-back message must be redelivered as a second run input; got {run_inputs:?}"
        );

        task.abort();
    }
}
