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
use llm_harness_types::RequiredToolCall;
use llm_harness_types::RunRequest;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::task::{AbortHandle, JoinHandle};
use tracing::{debug, error, info, warn};

use crate::AgentId;
use crate::GroupReplyContext;
use crate::controlled::{ControlledWorkerRun, DecisionWorkerRun};
use crate::inbox::{InboxMessage, MessageType};
use crate::social_context::{AgentStatus, SocialContext};
use futures::FutureExt;
use llm_harness_agent::session::types::{SessionEntry, SessionEntryPayload};
use llm_harness_types::{AgentMessage, ContentBlock, EntryId, UserMessage};

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
        let task_kind = match msg.task_kind {
            Some(kind) => format!(", task_kind: {}", kind.as_str()),
            None => String::new(),
        };
        out.push_str(&format!(
            "[from: {}, type: {}{correlation}{task_kind}] {}\n",
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

    if messages
        .iter()
        .any(|msg| msg.message_type == MessageType::Broadcast)
    {
        out.push_str(
            "\nBroadcast protocol: if this batch does not require a response or action from you, \
             reply with exactly <NO_RESPONSE>. The runtime treats this sentinel as silence: it \
             does not appear in chat and is removed from your session context.\n",
        );
    }

    out
}

/// Return the first explicit agent mention in a message body.
fn first_agent_mention(body: &str) -> Option<&str> {
    body.split_whitespace()
        .find_map(|word| word.strip_prefix('@').filter(|id| !id.is_empty()))
}

pub const NO_RESPONSE: &str = "<NO_RESPONSE>";

const PIPELINE_CONTROLLER: &str = "pipeline-controller";
const PIPELINE_TASK_PREFIX: &str = "task-pipe-";
const ORCHESTRATOR: &str = "orchestrator";

/// A controlled worker assignment gets one initial run and at most two
/// retries. This budget is per pipeline `task_id`, not per runner lifetime.
pub const MAX_CONTROLLED_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlledFailureCategory {
    ProviderOrHarnessFailure,
    MissingTerminalReport,
}

impl ControlledFailureCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderOrHarnessFailure => "provider_or_harness_failure",
            Self::MissingTerminalReport => "missing_terminal_report",
        }
    }
}

#[derive(Debug)]
struct ControlledAttempt {
    task_id: String,
    number: u32,
}

/// Shared exponential-delay calculation. Ordinary runs retain two immediate
/// retries; controlled assignments use the same calculation with no free
/// failures so their two bounded retries do not form a rapid loop.
fn retry_backoff_secs(failure_number: u32, free_failures: u32) -> u64 {
    let shift = failure_number.saturating_sub(free_failures + 1).min(6);
    (1u64 << shift).min(60)
}

/// Return `true` when the assignment was requeued. On exhaustion, consume the
/// original assignment and deliver a runtime-authored correlated ERROR so the
/// existing controller wait completes with a terminal non-success outcome.
fn handle_controlled_failure(
    ctx: &SocialContext,
    member: &AgentId,
    messages: Vec<InboxMessage>,
    attempt: &ControlledAttempt,
    category: ControlledFailureCategory,
    detail: &str,
) -> bool {
    let retries_remaining = MAX_CONTROLLED_ATTEMPTS.saturating_sub(attempt.number);
    let will_retry = retries_remaining > 0;
    if will_retry {
        let retry_delay_secs = retry_backoff_secs(attempt.number, 0);
        warn!(
            event = "controlled_task_retry",
            member = %member,
            task_id = %attempt.task_id,
            attempt = attempt.number,
            max_attempts = MAX_CONTROLLED_ATTEMPTS,
            reason = category.as_str(),
            retries_remaining,
            will_retry,
            retry_delay_secs,
            detail,
            "controlled task attempt failed; retry scheduled"
        );
        ctx.return_messages(member, messages);
        true
    } else {
        error!(
            event = "controlled_task_retry_exhausted",
            member = %member,
            task_id = %attempt.task_id,
            attempts = attempt.number,
            max_attempts = MAX_CONTROLLED_ATTEMPTS,
            reason = category.as_str(),
            retries_remaining,
            will_retry,
            detail,
            "controlled task retry budget exhausted"
        );
        let (assignment_messages, unrelated_messages): (Vec<_>, Vec<_>) =
            messages.into_iter().partition(|message| {
                message.message_type == MessageType::Direct
                    && message.from.as_str() == PIPELINE_CONTROLLER
                    && message.task_id.as_deref() == Some(attempt.task_id.as_str())
            });
        ctx.ack_inbox_messages(&assignment_messages);
        ctx.return_messages(member, unrelated_messages);
        let body = format!(
            "runtime execution failure: controlled task retry budget exhausted after {}/{} attempts; last_failure_category={}",
            attempt.number,
            MAX_CONTROLLED_ATTEMPTS,
            category.as_str(),
        );
        if let Err(delivery_error) = ctx.send_direct_with_task_kind(
            member,
            &AgentId::new(PIPELINE_CONTROLLER),
            body,
            10_000,
            Some(attempt.task_id.clone()),
            None,
            Some(crate::TaskMessageKind::Error),
        ) {
            error!(
                event = "controlled_task_retry_exhaustion_delivery_failed",
                member = %member,
                task_id = %attempt.task_id,
                error = %delivery_error,
                "failed to deliver runtime-authored controlled task error"
            );
        }
        false
    }
}

fn controlled_pipeline_task(messages: &[InboxMessage]) -> Option<&str> {
    messages.iter().find_map(|message| {
        (message.message_type == MessageType::Direct
            && message.from.as_str() == PIPELINE_CONTROLLER)
            .then_some(message.task_id.as_deref())
            .flatten()
            .filter(|task_id| task_id.starts_with(PIPELINE_TASK_PREFIX))
    })
}

/// The orchestrator's decision is its first action. Controlled workers instead
/// keep automatic tool choice so they can work before reporting completion.
fn required_reply_tool(agent_id: &AgentId, messages: &[InboxMessage]) -> Option<RequiredToolCall> {
    (agent_id.as_str() == ORCHESTRATOR && controlled_pipeline_task(messages).is_some())
        .then(|| RequiredToolCall::new("send_message"))
}

fn controlled_worker_run(
    agent_id: &AgentId,
    messages: &[InboxMessage],
) -> Option<ControlledWorkerRun> {
    (agent_id.as_str() != ORCHESTRATOR)
        .then(|| controlled_pipeline_task(messages))
        .flatten()
        .map(ControlledWorkerRun::new)
}

/// The Orchestrator's decision is its semantic terminal, not a typed worker
/// report. A decision-local receipt lets the runner seal execution once a
/// valid correlated ACTION is durably delivered, instead of waiting for the
/// harness to exit naturally (which may linger or emit a second ACTION).
fn decision_worker_run(
    agent_id: &AgentId,
    messages: &[InboxMessage],
) -> Option<DecisionWorkerRun> {
    (agent_id.as_str() == ORCHESTRATOR)
        .then(|| controlled_pipeline_task(messages))
        .flatten()
        .map(DecisionWorkerRun::new)
}

/// Only an explicit broadcast may establish a public-reply turn. Direct
/// messages can correlate with a prior broadcast, but they must not turn a
/// mixed batch into a restricted broadcast turn.
fn group_reply_context(messages: &[InboxMessage]) -> Option<GroupReplyContext> {
    let message = messages
        .iter()
        .rev()
        .find(|message| message.message_type == MessageType::Broadcast)?;
    let responder = first_agent_mention(&message.body).map(str::to_owned);
    Some(GroupReplyContext {
        broadcast_id: message.broadcast_id.clone()?,
        responder,
    })
}

/// A sentinel-only final answer is an explicit runtime decline.
fn is_no_response_message(message: &AgentMessage) -> bool {
    let AgentMessage::Assistant(message) = message else {
        return false;
    };
    !message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
        && message.text_content().trim() == NO_RESPONSE
}

/// Return the active-path leaf entry when it is a sentinel-only assistant reply.
fn final_no_response_entry(entries: &[SessionEntry]) -> Option<EntryId> {
    let entry = entries.last()?;
    if let SessionEntryPayload::Message(message) = &entry.payload
        && is_no_response_message(message)
    {
        Some(entry.id)
    } else {
        None
    }
}

/// Return the newest plain assistant text added during a run.
fn final_plain_text(entries: &[SessionEntry]) -> Option<String> {
    entries.iter().rev().find_map(|entry| {
        let SessionEntryPayload::Message(AgentMessage::Assistant(message)) = &entry.payload else {
            return None;
        };
        let has_tool_call = message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
        if has_tool_call {
            return None;
        }
        let text = message.text_content().trim().to_string();
        (!text.trim().is_empty() && text.trim() != NO_RESPONSE).then_some(text)
    })
}

/// Whether the run already delivered a public reply through a social tool.
fn public_reply_was_sent(entries: &[SessionEntry]) -> bool {
    entries.iter().any(|entry| {
        let SessionEntryPayload::Message(AgentMessage::Assistant(message)) = &entry.payload else {
            return false;
        };
        message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolUse { name, .. }
                    if name == "broadcast" || name == "send_message"
            )
        })
    })
}

async fn prior_entry_ids(harness: &AgentHarness) -> HashSet<EntryId> {
    harness
        .read_active_path()
        .await
        .map(|entries| entries.into_iter().map(|entry| entry.id).collect())
        .unwrap_or_default()
}

/// Remove a just-created sentinel response and report whether it was removed.
async fn discard_no_response_entry(harness: &AgentHarness) -> bool {
    let Ok(entries) = harness.read_active_path().await else {
        return false;
    };
    if let Some(entry_id) = final_no_response_entry(&entries) {
        match harness.delete_entries(vec![entry_id]).await {
            Ok(()) => return true,
            Err(error) => warn!(
                error = ?error,
                "failed to discard <NO_RESPONSE> from session context"
            ),
        }
    }
    false
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
    #[cfg(test)]
    continuation_probe: Option<Arc<ContinuationProbe>>,
}

/// Per-runner witness used only by the durable monitor regression below.
#[cfg(test)]
#[derive(Clone, Debug, Default)]
struct ContinuationObservation {
    disposition_reached: bool,
    salvage_flag: Option<bool>,
    queued_at_disposition: Option<bool>,
    entered: bool,
    result: Option<String>,
    /// Harness `error_message` captured synchronously when the salvage
    /// continuation returned — a retry run clears it, so reading it from
    /// the test afterwards races.
    internal_error_at_completion: Option<String>,
}

#[cfg(test)]
struct ContinuationProbe(parking_lot::Mutex<ContinuationObservation>);

#[cfg(test)]
impl ContinuationProbe {
    fn new() -> Arc<Self> {
        Arc::new(Self(parking_lot::Mutex::new(
            ContinuationObservation::default(),
        )))
    }

    fn snapshot(&self) -> ContinuationObservation {
        self.0.lock().clone()
    }

    fn entered(&self) {
        self.0.lock().entered = true;
    }

    fn disposition(&self, salvaged: bool, queued: bool) {
        let mut state = self.0.lock();
        // A failed continuation requeues the batch and the runner retries,
        // producing further dispositions. The test pins the CONTINUATION's
        // disposition, so the first observation wins.
        if state.salvage_flag.is_some() {
            return;
        }
        state.disposition_reached = true;
        state.salvage_flag = Some(salvaged);
        state.queued_at_disposition = Some(queued);
    }

    fn completed(
        &self,
        result: &Result<(), llm_harness_types::HarnessError>,
        internal_error: Option<String>,
    ) {
        let mut state = self.0.lock();
        state.result = Some(match result {
            Ok(()) => "Ok(())".to_owned(),
            Err(error) => format!("Err({error:?})"),
        });
        state.internal_error_at_completion = internal_error;
    }
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
            #[cfg(test)]
            continuation_probe: None,
        }
    }
    /// Spawn the self-driving loop as a tokio task and return its handle.
    pub fn spawn(self) -> JoinHandle<()> {
        let agent_id = self.agent_id.clone();
        let harness = self.harness.clone();
        let ctx = self.ctx.clone();
        let mut reflection = self.reflection;
        let knowledge_access = self.knowledge_access;
        #[cfg(test)]
        let continuation_probe = self.continuation_probe;

        tokio::spawn(async move {
            let mut consecutive_failures: u32 = 0;
            let mut controlled_attempts: HashMap<String, u32> = HashMap::new();
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
                if let Some(required) = required_reply_tool(&agent_id, &messages) {
                    request = request.with_extension(required);
                }
                let pipeline_task_id = controlled_pipeline_task(&messages).map(str::to_owned);
                let controlled_worker = controlled_worker_run(&agent_id, &messages);
                let decision_worker = decision_worker_run(&agent_id, &messages);
                let controlled_attempt = controlled_worker.as_ref().map(|controlled| {
                    let task_id = controlled.task_id().to_owned();
                    let number = controlled_attempts
                        .entry(task_id.clone())
                        .and_modify(|attempt| *attempt += 1)
                        .or_insert(1);
                    let attempt = ControlledAttempt {
                        task_id,
                        number: *number,
                    };
                    info!(
                        event = "controlled_task_attempt",
                        member = %agent_id,
                        task_id = %attempt.task_id,
                        attempt = attempt.number,
                        max_attempts = MAX_CONTROLLED_ATTEMPTS,
                        "controlled task attempt started"
                    );
                    attempt
                });
                if let Some(controlled) = &controlled_worker {
                    request = request.with_extension(controlled.clone());
                }
                if let Some(decision) = &decision_worker {
                    request = request.with_extension(decision.clone());
                }
                let pipeline_cancellation = pipeline_task_id.as_ref().and_then(|task_id| {
                    ctx.controlled_task_token(task_id)
                        .map(|token| (task_id.clone(), token))
                });
                if let Some((task_id, token)) = &pipeline_cancellation
                    && token.is_cancelled()
                {
                    info!(
                        event = "controlled_task_cancelled",
                        member = %agent_id,
                        task_id,
                        "cancelled controlled task will not start another attempt"
                    );
                    ctx.ack_inbox_messages(&messages);
                    ctx.complete_controlled_task(task_id);
                    controlled_attempts.remove(task_id);
                    ctx.update_status(&agent_id, AgentStatus::Idle);
                    continue;
                }
                let group_context = group_reply_context(&messages);
                let is_group_reply = group_context.is_some();
                let prior_entries = if group_context.is_some() {
                    prior_entry_ids(&harness).await
                } else {
                    HashSet::new()
                };
                if let Some(group) = group_context {
                    request = request.with_extension(group);
                }
                if let Some(access) = &knowledge_access {
                    // T15: knowledge/memory tools resolve their
                    // KnowledgeRequestContext from run extensions; without it
                    // every call fails with KnowledgeError::Unauthorized.
                    request = request.with_extension((**access).clone());
                }

                // Spawn monitor task: drain inbox → steer during run.
                let monitor = spawn_monitor_task(agent_id.clone(), harness.clone(), ctx.clone());
                let mut no_response_run = false;

                // AssertUnwindSafe: the harness and ctx are Arc-backed shared
                // state that stays consistent across a panic (worse case: a
                // half-applied turn, which the next run overwrites). Without
                // this, one panicking run kills the runner task and leaves
                // the agent permanently Busy in the team directory.
                let run_future = std::panic::AssertUnwindSafe(harness.run(request)).catch_unwind();
                tokio::pin!(run_future);
                let terminal_delivery = controlled_worker
                    .as_ref()
                    .map(ControlledWorkerRun::terminal_delivered);
                let decision_delivery = decision_worker
                    .as_ref()
                    .map(DecisionWorkerRun::decision_delivered);
                let mut terminal_sealed = false;
                let run_result = if let Some(delivered) = decision_delivery {
                    // Decision run: a valid correlated ACTION seals execution.
                    // The receipt only fires after the controller grammar
                    // accepts the body, and send_message records it only
                    // after durable delivery.
                    if let Some((task_id, token)) = &pipeline_cancellation {
                        tokio::select! {
                            biased;
                            _ = delivered.cancelled() => {
                                info!(
                                    event = "controlled_task_terminal_sealed",
                                    member = %agent_id,
                                    task_id = %task_id,
                                    "durable decision ACTION sealed orchestrator run"
                                );
                                // The decision is already durable. Abort and
                                // drop the still-active run future so a later
                                // provider turn or tool call (e.g. a second
                                // ACTION) cannot keep the runner Busy after
                                // the decision was accepted.
                                harness.abort();
                                None
                            }
                            _ = token.cancelled() => {
                                info!(
                                    event = "controlled_task_cancelled",
                                    member = %agent_id,
                                    task_id,
                                    "cancelling active controlled harness run"
                                );
                                harness.abort();
                                let _ = run_future.await;
                                None
                            }
                            result = &mut run_future => Some(result),
                        }
                    } else {
                        tokio::select! {
                            biased;
                            _ = delivered.cancelled() => {
                                info!(
                                    event = "controlled_task_terminal_sealed",
                                    member = %agent_id,
                                    task_id = decision_worker.as_ref().unwrap().task_id(),
                                    "durable decision ACTION sealed orchestrator run"
                                );
                                harness.abort();
                                None
                            }
                            result = &mut run_future => Some(result),
                        }
                    }
                } else if let Some(delivered) = terminal_delivery {
                    if let Some((task_id, token)) = &pipeline_cancellation {
                        tokio::select! {
                            biased;
                            _ = delivered.cancelled() => {
                                terminal_sealed = true;
                                info!(
                                    event = "controlled_task_terminal_sealed",
                                    member = %agent_id,
                                    task_id = %task_id,
                                    "durable terminal report sealed controlled worker run"
                                );
                                // The report is already durable. Abort and
                                // drop the still-active run future so a hook,
                                // provider continuation, or later tool cannot
                                // keep the runner Busy after task completion.
                                harness.abort();
                                None
                            }
                            _ = token.cancelled() => {
                                info!(
                                    event = "controlled_task_cancelled",
                                    member = %agent_id,
                                    task_id,
                                    "cancelling active controlled harness run"
                                );
                                harness.abort();
                                let _ = run_future.await;
                                None
                            }
                            result = &mut run_future => Some(result),
                        }
                    } else {
                        tokio::select! {
                            biased;
                            _ = delivered.cancelled() => {
                                terminal_sealed = true;
                                info!(
                                    event = "controlled_task_terminal_sealed",
                                    member = %agent_id,
                                    task_id = controlled_worker.as_ref().unwrap().task_id(),
                                    "durable terminal report sealed controlled worker run"
                                );
                                harness.abort();
                                None
                            }
                            result = &mut run_future => Some(result),
                        }
                    }
                } else if let Some((task_id, token)) = &pipeline_cancellation {
                    tokio::select! {
                        result = &mut run_future => Some(result),
                        _ = token.cancelled() => {
                            info!(
                                event = "controlled_task_cancelled",
                                member = %agent_id,
                                task_id,
                                "cancelling active controlled harness run"
                            );
                            harness.abort();
                            let _ = run_future.await;
                            None
                        }
                    }
                } else {
                    Some(run_future.await)
                };

                // Abort the monitor task — run is over.
                monitor.abort();

                // Preserve the work turn's outcome before a corrective
                // finalization run can replace the harness state.
                let initial_run_error = harness.state().error_message;

                let mut retry_delay_secs = None;
                let mut controlled_reported = controlled_worker
                    .as_ref()
                    .is_some_and(ControlledWorkerRun::was_reported);
                let mut pipeline_cancelled = run_result.is_none() && !terminal_sealed;
                let decision_accepted = decision_worker
                    .as_ref()
                    .is_some_and(DecisionWorkerRun::was_decided);

                // A normally completed worker run gets exactly one corrective
                // turn in the same persistent harness session. This turn does
                // not repeat the assignment; it can only report the outcome.
                // Decision runs (Orchestrator) have their own sealed terminal
                // semantics and never take the worker corrective path.
                if !controlled_reported
                    && matches!(&run_result, Some(Ok(Ok(()))))
                    && initial_run_error.is_none()
                    && let (Some(controlled), Some(attempt)) =
                        (&controlled_worker, &controlled_attempt)
                {
                    info!(
                        event = "controlled_task_finalization_required",
                        member = %agent_id,
                        task_id = %attempt.task_id,
                        attempt = attempt.number,
                        "controlled worker completed work without a terminal report"
                    );
                    let prompt = format!(
                        "The work phase is complete. Do not repeat the assignment or perform more work. Report the outcome now with send_message to {PIPELINE_CONTROLLER}, using exact task_id={:?}. Set task_kind to result, blocked, or error, and include nonempty evidence, the blocking reason, or error detail in body.",
                        attempt.task_id
                    );
                    let mut finalization = RunRequest::from_text(prompt)
                        .with_extension(RequiredToolCall::new("send_message"))
                        .with_extension(controlled.clone());
                    if let Some(access) = &knowledge_access {
                        finalization = finalization.with_extension((**access).clone());
                    }
                    let finalization_monitor =
                        spawn_monitor_task(agent_id.clone(), harness.clone(), ctx.clone());
                    let finalization_future =
                        std::panic::AssertUnwindSafe(harness.run(finalization)).catch_unwind();
                    tokio::pin!(finalization_future);
                    let finalization_terminal = controlled.terminal_delivered();
                    let mut finalization_sealed = false;
                    let finalization_completed = if let Some((task_id, token)) =
                        &pipeline_cancellation
                    {
                        tokio::select! {
                            biased;
                            _ = finalization_terminal.cancelled() => {
                                finalization_sealed = true;
                                info!(event = "controlled_task_terminal_sealed", member = %agent_id, task_id = %attempt.task_id, "durable finalization report sealed controlled worker run");
                                harness.abort();
                                true
                            }
                            _ = token.cancelled() => {
                                info!(event = "controlled_task_cancelled", member = %agent_id, task_id, "cancelling controlled finalization turn");
                                harness.abort();
                                let _ = finalization_future.await;
                                false
                            }
                            _ = &mut finalization_future => true,
                        }
                    } else {
                        tokio::select! {
                            biased;
                            _ = finalization_terminal.cancelled() => {
                                finalization_sealed = true;
                                info!(event = "controlled_task_terminal_sealed", member = %agent_id, task_id = %attempt.task_id, "durable finalization report sealed controlled worker run");
                                harness.abort();
                                true
                            }
                            _ = &mut finalization_future => true,
                        }
                    };
                    finalization_monitor.abort();
                    pipeline_cancelled = !finalization_completed && !finalization_sealed;
                    controlled_reported = controlled.was_reported();
                    if controlled_reported {
                        info!(
                            event = "controlled_task_finalization_succeeded",
                            member = %agent_id,
                            task_id = %attempt.task_id,
                            attempt = attempt.number,
                            "controlled worker supplied its terminal report"
                        );
                    } else if finalization_completed {
                        warn!(
                            event = "controlled_task_finalization_failed",
                            member = %agent_id,
                            task_id = %attempt.task_id,
                            attempt = attempt.number,
                            "corrective finalization did not supply a terminal report"
                        );
                    }
                }

                // Single salvage take: after any corrective finalization run
                // has finished (it consumed the buffered salvage as its own
                // initial messages and may set a fresh flag), read and clear
                // the flag exactly once for this batch's disposition. A take
                // before finalization would miss finalization-produced
                // salvage and leak a stale flag into the next unrelated
                // batch.
                let unconsumed_steers_salvaged = harness.take_salvaged_steers();
                #[cfg(test)]
                if let Some(probe) = &continuation_probe {
                    probe.disposition(unconsumed_steers_salvaged, harness.has_queued_messages());
                }

                if decision_accepted {
                    // A valid correlated ACTION is the decision terminal even
                    // if the sealed harness reported a later run-level error.
                    // The assignment is consumed: the controller already owns
                    // the authoritative decision.
                    if let Some(task_id) = &pipeline_task_id {
                        controlled_attempts.remove(task_id);
                    }
                    consecutive_failures = 0;
                    ctx.ack_inbox_messages(&messages);
                    if let Some(task_id) = &pipeline_task_id {
                        ctx.complete_controlled_task(task_id);
                    }
                } else if controlled_reported {
                    // A valid correlated RESULT/BLOCKED/ERROR is terminal even
                    // if the harness reports a later run-level error.
                    if let Some(attempt) = &controlled_attempt {
                        controlled_attempts.remove(&attempt.task_id);
                    }
                    consecutive_failures = 0;
                    ctx.ack_inbox_messages(&messages);
                    if let Some(attempt) = &controlled_attempt {
                        ctx.complete_controlled_task(&attempt.task_id);
                    }
                } else if pipeline_cancelled {
                    // Workflow ownership was revoked while a worker or
                    // Orchestrator decision was in flight. Consume this
                    // assignment and never requeue it.
                    if let Some(task_id) = &pipeline_task_id {
                        controlled_attempts.remove(task_id);
                        ctx.complete_controlled_task(task_id);
                    }
                    consecutive_failures = 0;
                    ctx.ack_inbox_messages(&messages);
                } else {
                    match run_result.expect("cancelled run handled above") {
                        Ok(Ok(())) => {
                            // The run "completed" but the harness may have
                            // recorded an internal error mid-run
                            // (error_message — e.g. a provider failure surfaced
                            // as AgentEvent::Error, which drive_loop converts to
                            // Ok). Re-queue the consumed messages so they aren't
                            // silently lost; duplicate delivery beats loss.
                            // Aborts do not set error_message, so this never
                            // rolls back an aborted run.
                            if let Some(attempt) = &controlled_attempt {
                                let (category, detail) = match initial_run_error.as_deref() {
                                    Some(run_error) => (
                                        ControlledFailureCategory::ProviderOrHarnessFailure,
                                        run_error,
                                    ),
                                    None => (
                                        ControlledFailureCategory::MissingTerminalReport,
                                        "harness run completed without a valid correlated terminal report",
                                    ),
                                };
                                let will_retry = handle_controlled_failure(
                                    &ctx, &agent_id, messages, attempt, category, detail,
                                );
                                if will_retry {
                                    consecutive_failures += 1;
                                    retry_delay_secs = Some(retry_backoff_secs(attempt.number, 0));
                                } else {
                                    consecutive_failures = 0;
                                    controlled_attempts.remove(&attempt.task_id);
                                }
                            } else if initial_run_error.is_some() {
                                consecutive_failures += 1;
                                error!(
                                    runner = %agent_id,
                                    error = ?initial_run_error,
                                    "harness run failed after consuming messages; re-queuing"
                                );
                                ctx.return_messages(&agent_id, messages);
                            } else {
                                // Ordinary success: the batch is consumed at
                                // exactly this boundary. If the harness
                                // salvaged unconsumed steers this run, those
                                // steers are already buffered as this
                                // harness's next-turn messages; a continuation
                                // run here delivers them to the model exactly
                                // once WITHOUT a social-inbox requeue, so the
                                // same InboxMessage can never be delivered
                                // twice (the old requeue also stacked with
                                // the ack below, double-scheduling the
                                // message). If no salvage occurred, this run
                                // is a no-op that preserves the old behavior
                                // of acking a finished batch in place.
                                if unconsumed_steers_salvaged {
                                    let continuation =
                                        RunRequest::from_text(String::new());
                                    #[cfg(test)]
                                    if let Some(probe) = &continuation_probe {
                                        probe.entered();
                                    }
                                    let continuation_result = harness.run(continuation).await;
                                    let continuation_internal_error =
                                        harness.state().error_message.clone();
                                    #[cfg(test)]
                                    if let Some(probe) = &continuation_probe {
                                        probe.completed(
                                            &continuation_result,
                                            continuation_internal_error.clone(),
                                        );
                                    }
                                    // Mirror the main-run path: a provider
                                    // error surfaced as AgentEvent::Error is
                                    // reported by drive_loop as Ok(()) with
                                    // harness error_message set, so a bare
                                    // result check misses it. Acking here
                                    // would delete the journal rows while the
                                    // delivered-once salvage sits in a
                                    // volatile buffer with no live wakeup —
                                    // a crash between the ack and the next
                                    // run start loses the whole batch.
                                    let continuation_failed = continuation_result.is_err()
                                        || continuation_internal_error.is_some();
                                    if continuation_failed {
                                        // The salvage buffer survives a failed
                                        // continuation (queued_next_turn is
                                        // only cleared on a successful run()
                                        // start). Skip the ack so the journal
                                        // rows stay intact for redelivery, and
                                        // return the batch so the next runner
                                        // iteration retries it together with
                                        // the buffered salvage. Fall through to
                                        // the ordinary loop tail (idle check,
                                        // backoff) — never leave the agent Busy
                                        // or kill the runner task.
                                        error!(
                                            runner = %agent_id,
                                            error = ?continuation_result,
                                            internal_error = ?harness.state().error_message,
                                            "salvage continuation run failed; re-queuing batch"
                                        );
                                        consecutive_failures += 1;
                                        ctx.return_messages(&agent_id, messages);
                                        ctx.update_status(&agent_id, AgentStatus::Idle);
                                        continue;
                                    }
                                }
                                consecutive_failures = 0;
                                if !is_reflection_run {
                                    no_response_run = discard_no_response_entry(&harness).await;
                                }
                                if is_group_reply
                                    && let Ok(entries) = harness.read_active_path().await
                                {
                                    let new_entries = entries
                                        .into_iter()
                                        .filter(|entry| !prior_entries.contains(&entry.id))
                                        .collect::<Vec<_>>();
                                    if !public_reply_was_sent(&new_entries)
                                        && let Some(body) = final_plain_text(&new_entries)
                                    {
                                        // Deliver as a direct reply to the operator
                                        // carrying the ORIGINATING broadcast_id
                                        // (renders in the group-reply view), never
                                        // as a fresh team-wide broadcast: a new
                                        // Broadcast message establishes a new
                                        // group-reply turn on every other member,
                                        // so one generic message would recursively
                                        // trigger one run per member per round —
                                        // the runaway loop this regression closes.
                                        let _ = ctx.send_direct_with_task(
                                            &agent_id,
                                            &AgentId::new(crate::reserved::OPERATOR),
                                            body,
                                            0,
                                            None,
                                            Some(
                                                group_reply_context(&messages)
                                                    .expect(
                                                        "is_group_reply implies a broadcast context",
                                                    )
                                                    .broadcast_id,
                                            ),
                                        );
                                    }
                                }
                                ctx.ack_inbox_messages(&messages);
                            }
                        }
                        Ok(Err(e)) => {
                            // Run failed: the drained messages were never
                            // processed. Return them to the inbox so the next
                            // run (or a retried run) handles them, instead of
                            // silently swallowing incoming work.
                            if let Some(attempt) = &controlled_attempt {
                                let detail = format!("{e:?}");
                                let will_retry = handle_controlled_failure(
                                    &ctx,
                                    &agent_id,
                                    messages,
                                    attempt,
                                    ControlledFailureCategory::ProviderOrHarnessFailure,
                                    &detail,
                                );
                                if will_retry {
                                    consecutive_failures += 1;
                                    retry_delay_secs = Some(retry_backoff_secs(attempt.number, 0));
                                } else {
                                    consecutive_failures = 0;
                                    controlled_attempts.remove(&attempt.task_id);
                                }
                            } else {
                                error!(agent = %agent_id, error = ?e, "harness run failed");
                                consecutive_failures += 1;
                                ctx.return_messages(&agent_id, messages);
                            }
                        }
                        Err(panic) => {
                            // Run panicked: same message-loss hazard as above,
                            // plus the status must not stay Busy.
                            let msg = panic
                                .downcast_ref::<&str>()
                                .map(|s| s.to_string())
                                .or_else(|| panic.downcast_ref::<String>().cloned())
                                .unwrap_or_else(|| "unknown panic".into());
                            if let Some(attempt) = &controlled_attempt {
                                let will_retry = handle_controlled_failure(
                                    &ctx,
                                    &agent_id,
                                    messages,
                                    attempt,
                                    ControlledFailureCategory::ProviderOrHarnessFailure,
                                    &msg,
                                );
                                if will_retry {
                                    consecutive_failures += 1;
                                    retry_delay_secs = Some(retry_backoff_secs(attempt.number, 0));
                                } else {
                                    consecutive_failures = 0;
                                    controlled_attempts.remove(&attempt.task_id);
                                }
                            } else {
                                error!(agent = %agent_id, panic = %msg, "harness run panicked");
                                consecutive_failures += 1;
                                ctx.return_messages(&agent_id, messages);
                            }
                        }
                    }
                }

                // ── Exponential backoff ───────────────────────────────────
                // Re-queueing makes has_messages() true immediately, so a
                // persistently failing run would hot-spin (fail → wake →
                // fail). Controlled failures set a bounded 1s/2s delay above;
                // ordinary runs preserve the existing policy of sleeping
                // min(2^(n-3), 60) seconds from failure 3 onward. Any success
                // resets the counter.
                if retry_delay_secs.is_none() && consecutive_failures >= 3 {
                    retry_delay_secs = Some(retry_backoff_secs(consecutive_failures, 2));
                }
                if let Some(secs) = retry_delay_secs {
                    warn!(
                        agent = %agent_id,
                        consecutive_failures,
                        backoff_secs = secs,
                        "harness run keeps failing; backing off before retry"
                    );
                    if let Some((task_id, token)) = &pipeline_cancellation {
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
                            _ = token.cancelled() => {
                                info!(
                                    event = "controlled_task_cancelled",
                                    member = %agent_id,
                                    task_id,
                                    "cancelled pipeline decision will not wait for or start another retry"
                                );
                            }
                        }
                    } else {
                        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                    }
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
                    } else if !no_response_run {
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
                            if let Err(error) = ctx.push_message(InboxMessage {
                                journal_id: None,
                                from: agent_id.clone(),
                                to: agent_id.clone(),
                                body: task_text,
                                message_type: MessageType::ReflectionTrigger,
                                priority: crate::inbox::priority::REFLECTION,
                                task_id: None,
                                task_kind: None,
                                broadcast_id: None,
                                message_id: None,
                            }) {
                                error!(
                                    agent = %agent_id,
                                    error = %error,
                                    "failed to journal reflection trigger"
                                );
                            }
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
#[allow(unused_must_use)]
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
                message_id: None,
                task_id: None,
                task_kind: None,
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
                task_kind: None,
                broadcast_id: None,
                message_id: None,
            },
        ];
        let text = format_inbox_messages(&msgs);
        assert!(text.contains("Direct"));
        assert!(text.contains("Timeout"));
        assert!(text.contains("hello"));
        assert!(text.contains("timeout!"));
    }

    #[test]
    fn format_inbox_messages_preserves_task_semantics() {
        let mut message = private_message("halfway done");
        message.task_id = Some("task-1".into());
        message.task_kind = Some(crate::TaskMessageKind::Progress);

        let text = format_inbox_messages(&[message]);

        assert!(text.contains("task: task-1"));
        assert!(text.contains("task_kind: progress"));
    }

    #[test]
    fn format_inbox_messages_declares_no_response_protocol_for_broadcasts() {
        let msgs = vec![broadcast_message("general update")];
        let text = format_inbox_messages(&msgs);

        assert!(text.contains("<NO_RESPONSE>"));
        assert!(text.contains("removed from your session context"));
    }

    #[test]
    fn no_response_protocol_omitted_for_private_messages() {
        let msgs = vec![private_message("hello")];
        let text = format_inbox_messages(&msgs);

        assert!(!text.contains("<NO_RESPONSE>"));
    }

    #[test]
    fn controlled_orchestrator_decision_requires_send_message() {
        let mut message = private_message("make the next decision");
        message.from = AgentId::new(PIPELINE_CONTROLLER);
        message.task_id = Some("task-pipe-issue-7-decision-1".into());

        let required = required_reply_tool(&AgentId::new(ORCHESTRATOR), &[message])
            .expect("controlled orchestrator decision must be enforced");

        assert_eq!(required.tool_name(), "send_message");
    }

    #[test]
    fn controlled_worker_keeps_automatic_tool_choice_and_tracks_result() {
        let mut message = private_message("implement and test");
        message.from = AgentId::new(PIPELINE_CONTROLLER);
        message.task_id = Some("task-pipe-issue-7-worker-1".into());
        let coder = AgentId::new("coder");

        assert!(required_reply_tool(&coder, std::slice::from_ref(&message)).is_none());
        assert!(controlled_worker_run(&coder, &[message]).is_some());
    }

    #[test]
    fn ordinary_messages_do_not_require_send_message() {
        let ordinary = private_message("hello");
        let mut unrelated_task = private_message("unrelated task");
        unrelated_task.from = AgentId::new(PIPELINE_CONTROLLER);
        unrelated_task.task_id = Some("other-task-7".into());
        let mut broadcast = broadcast_message("team update");
        broadcast.from = AgentId::new(PIPELINE_CONTROLLER);
        broadcast.task_id = Some("task-pipe-issue-7-decision-1".into());

        let orchestrator = AgentId::new(ORCHESTRATOR);
        assert!(required_reply_tool(&orchestrator, &[ordinary]).is_none());
        assert!(required_reply_tool(&orchestrator, &[unrelated_task]).is_none());
        assert!(required_reply_tool(&orchestrator, &[broadcast]).is_none());
    }

    #[test]
    fn final_no_response_entry_detects_sentinel_only_assistant() {
        let sentinel = assistant_entry(NO_RESPONSE, None);
        assert_eq!(
            final_no_response_entry(std::slice::from_ref(&sentinel)),
            Some(sentinel.id)
        );

        let tool_use = assistant_entry(
            "",
            Some(ContentBlock::ToolUse {
                id: "tool-1".into(),
                name: "send_message".into(),
                input: serde_json::json!({"to": "operator", "body": "done"}),
            }),
        );
        assert_eq!(
            final_no_response_entry(std::slice::from_ref(&tool_use)),
            None
        );

        let real_reply = assistant_entry("here is the answer", None);
        assert_eq!(
            final_no_response_entry(std::slice::from_ref(&real_reply)),
            None
        );
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
                task_kind: None,
                broadcast_id: None,
                message_id: None,
            },
            InboxMessage {
                journal_id: None,
                from: AgentId::new("bob"),
                to: AgentId::new("alice"),
                body: "I can help with this".into(),
                message_type: MessageType::Direct,
                priority: 0,
                task_id: None,
                task_kind: None,
                broadcast_id: Some("bid-123".into()),
                message_id: None,
            },
            InboxMessage {
                journal_id: None,
                from: AgentId::new("carol"),
                to: AgentId::new("alice"),
                body: "I have experience here".into(),
                message_type: MessageType::Direct,
                priority: 0,
                task_id: None,
                task_kind: None,
                broadcast_id: Some("bid-123".into()),
                message_id: None,
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
                task_kind: None,
                broadcast_id: Some("bid-x".into()),
                message_id: None,
            },
            InboxMessage {
                journal_id: None,
                from: AgentId::new("dave"),
                to: AgentId::new("alice"),
                body: "please take this task".into(),
                message_type: MessageType::DelegationRequest,
                priority: 0,
                task_id: Some("t9".into()),
                task_kind: None,
                broadcast_id: Some("bid-x".into()),
                message_id: None,
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

    fn message(
        from: &str,
        to: &str,
        body: &str,
        message_type: MessageType,
        broadcast_id: Option<&str>,
    ) -> InboxMessage {
        InboxMessage {
            journal_id: None,
            from: AgentId::new(from),
            to: AgentId::new(to),
            body: body.into(),
            message_type,
            priority: 0,
            task_id: None,
            task_kind: None,
            broadcast_id: broadcast_id.map(str::to_owned),
            message_id: None,
        }
    }

    #[test]
    fn group_context_comes_only_from_explicit_broadcast() {
        let messages = [message(
            "operator",
            "alice",
            "@bob please lead this",
            MessageType::Broadcast,
            Some("broadcast-1"),
        )];
        let context = group_reply_context(&messages).unwrap();
        assert_eq!(context.broadcast_id, "broadcast-1");
        assert_eq!(context.responder.as_deref(), Some("bob"));
    }

    #[test]
    fn mixed_direct_batch_does_not_leak_group_context() {
        let messages = [
            message(
                "carol",
                "alice",
                "private request",
                MessageType::Direct,
                Some("stale-broadcast"),
            ),
            message(
                "operator",
                "alice",
                "@bob public request",
                MessageType::Broadcast,
                Some("broadcast-1"),
            ),
        ];
        let context = group_reply_context(&messages).unwrap();
        assert_eq!(context.broadcast_id, "broadcast-1");
        assert_eq!(context.responder.as_deref(), Some("bob"));

        let messages = [message(
            "carol",
            "alice",
            "private request",
            MessageType::Direct,
            Some("stale-broadcast"),
        )];
        assert!(group_reply_context(&messages).is_none());
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
                task_kind: None,
                broadcast_id: Some(bid.into()),
                message_id: None,
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
                task_kind: None,
                broadcast_id: None,
                message_id: None,
            },
            InboxMessage {
                journal_id: None,
                from: AgentId::new("y"),
                to: AgentId::new("alice"),
                body: "r1\nr2".into(),
                message_type: MessageType::Direct,
                priority: 0,
                task_id: None,
                task_kind: None,
                broadcast_id: Some("bid-m".into()),
                message_id: None,
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

    fn broadcast_message(body: &str) -> InboxMessage {
        InboxMessage {
            journal_id: None,
            from: AgentId::new("operator"),
            to: AgentId::new("alice"),
            body: body.into(),
            message_type: MessageType::Broadcast,
            priority: crate::inbox::priority::NORMAL,
            task_id: None,
            task_kind: None,
            broadcast_id: Some("broadcast-1".into()),
            message_id: None,
        }
    }

    fn private_message(body: &str) -> InboxMessage {
        InboxMessage {
            journal_id: None,
            from: AgentId::new("operator"),
            to: AgentId::new("alice"),
            body: body.into(),
            message_type: MessageType::Direct,
            priority: crate::inbox::priority::OPERATOR,
            task_id: None,
            task_kind: None,
            broadcast_id: None,
            message_id: None,
        }
    }

    fn assistant_entry(text: &str, extra_block: Option<ContentBlock>) -> SessionEntry {
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentBlock::Text { text: text.into() });
        }
        if let Some(block) = extra_block {
            content.push(block);
        }
        SessionEntry {
            id: llm_harness_types::EntryId::new(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            payload: SessionEntryPayload::Message(AgentMessage::Assistant(
                llm_harness_types::AssistantMessage {
                    kind: llm_harness_types::AssistantMessageKind::FinalAnswer,
                    message_id: uuid::Uuid::now_v7().to_string(),
                    turn_id: uuid::Uuid::now_v7().to_string(),
                    content,
                    stop_reason: None,
                    timestamp: chrono::Utc::now(),
                    provider: None,
                    api: None,
                    model: None,
                    usage: None,
                    error_message: None,
                },
            )),
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
            task_kind: None,
            broadcast_id: None,
            message_id: None,
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
            task_kind: None,
            broadcast_id: None,
            message_id: None,
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
            task_kind: None,
            broadcast_id: None,
            message_id: None,
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
            task_kind: None,
            broadcast_id: None,
            message_id: None,
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
            task_kind: None,
            broadcast_id: None,
            message_id: None,
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
    async fn no_response_is_removed_from_session_context() {
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
                    llm_harness_loop::test_utils::MockResponse::text(NO_RESPONSE),
                ])) as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                llm_harness_agent::AgentHarnessOptions::new("test-model"),
            )
            .await,
        );
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");
        ctx.push_message(broadcast_message("information for another member"));

        let runner = AgentRunner::new(alice.clone(), harness.clone(), ctx.clone(), None, None);
        let task = runner.spawn();
        for _ in 0..200 {
            if ctx.get_agent_info(&alice).map(|info| info.status) == Some(AgentStatus::Idle)
                && !ctx.has_messages(&alice)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert!(
            !task.is_finished(),
            "sentinel decline must not kill the runner"
        );
        assert_eq!(
            ctx.get_agent_info(&alice).unwrap().status,
            AgentStatus::Idle
        );
        assert!(!ctx.has_messages(&alice));
        let entries = harness.read_active_path().await.unwrap();
        assert!(
            !entries
                .iter()
                .any(|entry| final_no_response_entry(std::slice::from_ref(entry)).is_some()),
            "sentinel reply must not enter session context"
        );
        assert!(
            entries.iter().any(|entry| matches!(
                &entry.payload,
                SessionEntryPayload::Message(AgentMessage::User(message))
                    if message.content.iter().any(|block| matches!(
                        block,
                        ContentBlock::Text { text }
                            if text.contains("information for another member")
                    ))
            )),
            "the triggering group message should remain in context"
        );
        assert!(
            !ctx.group_history(10)
                .iter()
                .any(|message| message.body == NO_RESPONSE),
            "sentinel reply must not enter public chat history"
        );
    }

    #[tokio::test]
    async fn plain_final_answer_to_broadcast_becomes_public_reply() {
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
                    llm_harness_loop::test_utils::MockResponse::text("plain final answer"),
                ])) as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                llm_harness_agent::AgentHarnessOptions::new("test-model"),
            )
            .await,
        );
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(AgentId::new("operator"), "operator");
        ctx.push_message(broadcast_message("please answer"));

        let runner = AgentRunner::new(alice.clone(), harness.clone(), ctx.clone(), None, None);
        let task = runner.spawn();
        for _ in 0..200 {
            if ctx.get_agent_info(&alice).map(|info| info.status) == Some(AgentStatus::Idle)
                && !ctx.has_messages(&alice)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert!(
            ctx.group_history(10)
                .iter()
                .any(|message| message.from == alice && message.body == "plain final answer")
        );
        task.abort();
    }

    /// Regression (runaway Agent Team loop): the runner's auto-publish for a
    /// plain-text answer to a broadcast used `ctx.broadcast`, i.e. it created
    /// a NEW Broadcast message for every other member. Every recipient then
    /// ran a new group-reply turn, whose own plain answer became another
    /// broadcast — one generic message recursively triggered one run per
    /// member per round and never quiesced ("The operator sent a broadcast…
    /// Should I respond?…").
    ///
    /// Contract after the fix: the fallback reply is a DIRECT message to the
    /// operator carrying the ORIGINATING broadcast_id. It must (a) not be a
    /// Broadcast (no new group-reply turn on any member), and (b) stay
    /// visible in the group-reply view (broadcast_id preserved).
    #[tokio::test]
    async fn broadcast_fallback_reply_does_not_create_a_new_broadcast() {
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
                    llm_harness_loop::test_utils::MockResponse::text("plain final answer"),
                ])) as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                llm_harness_agent::AgentHarnessOptions::new("test-model"),
            )
            .await,
        );
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        let operator = AgentId::new(crate::timeout_manager::reserved::OPERATOR);
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "review");
        ctx.register_agent(operator.clone(), "operator");

        // Deliver a real broadcast through the production path: both members
        // receive it (this is exactly what the operator chat route does).
        let ctx_broadcast_id = ctx.broadcast(&operator, "please answer").unwrap();
        // Consume bob's ORIGINAL copy (bob has no runner, so it stays pending
        // and would otherwise be misread as a loop-produced broadcast below).
        let bob_original = ctx.drain_inbox(&bob);
        assert_eq!(bob_original.len(), 1);
        assert_eq!(
            bob_original[0].broadcast_id.as_deref(),
            Some(ctx_broadcast_id.as_str())
        );

        let runner = AgentRunner::new(alice.clone(), harness.clone(), ctx.clone(), None, None);
        let task = runner.spawn();
        // Wait until the fallback reply actually lands (bounded); breaking on
        // idle/empty inboxes races the runner startup.
        let mut reply_seen = false;
        for _ in 0..400 {
            if ctx
                .group_history(10)
                .iter()
                .any(|message| message.from == alice && message.body == "plain final answer")
            {
                reply_seen = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        task.abort();
        assert!(
            reply_seen,
            "fallback reply must be published within the bounded wait"
        );

        // (a) No new Broadcast message exists: a second broadcast would push a
        // group-reply turn into bob's inbox, re-arming the loop.
        let bob_inbox = ctx.drain_inbox(&bob);
        assert!(
            !bob_inbox
                .iter()
                .any(|message| message.message_type == MessageType::Broadcast),
            "fallback reply must never broadcast: bob must not receive a new group-reply turn"
        );
        // (b) The reply stays a correlated group reply: direct to the
        // operator, carrying the originating broadcast_id (group-reply view).
        let history = ctx.group_history(10);
        let reply = history
            .iter()
            .find(|message| message.from == alice && message.body == "plain final answer")
            .expect("fallback reply must remain visible in the group-reply view");
        assert_eq!(reply.message_type, MessageType::Direct);
        assert_eq!(reply.to, operator);
        assert_eq!(
            reply.broadcast_id,
            Some(ctx_broadcast_id),
            "reply must carry the ORIGINATING broadcast_id, not a fresh one"
        );
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
            task_kind: None,
            broadcast_id: None,
            message_id: None,
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

    #[tokio::test]
    async fn controlled_plain_text_is_retried_until_correlated_send_message() {
        use llm_harness_agent::Plugin;
        use llm_harness_strategy::status_panel::TodoStore;

        let task_id = "task-pipe-issue-7-decision-1";
        let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
            // Attempt 1 ends with a plain (uncorrelated-tool) ACTION text; the
            // runner must reject it and retry. Attempt 2 delivers the same
            // ACTION through the required correlated send_message, which
            // seals the run.
            llm_harness_loop::test_utils::MockResponse::text("ACTION: FINISH\nroger"),
            llm_harness_loop::test_utils::MockResponse::tool_use_end_turn(
                "reply-1",
                "send_message",
                &serde_json::json!({
                    "to": PIPELINE_CONTROLLER,
                    "task_id": task_id,
                    "body": "ACTION: FINISH\nroger",
                })
                .to_string(),
            ),
        ]));
        let ctx = SocialContext::new(10);
        let orchestrator = AgentId::new("orchestrator");
        let controller = AgentId::new(PIPELINE_CONTROLLER);
        ctx.register_agent(orchestrator.clone(), "workflow routing");
        ctx.register_agent(controller.clone(), "controller");
        ctx.ensure_contact(&orchestrator, &controller);

        let plugin =
            crate::SocialToolPlugin::new(orchestrator.clone(), ctx.clone(), TodoStore::new());
        let mut options = llm_harness_agent::AgentHarnessOptions::new("test-model");
        plugin.register_tools(&mut options.tools);
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                client.clone() as Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                options,
            )
            .await,
        );
        ctx.send_direct_with_task(
            &controller,
            &orchestrator,
            "choose the next action",
            0,
            Some(task_id.into()),
            None,
        )
        .unwrap();

        let runner = AgentRunner::new(orchestrator.clone(), harness, ctx.clone(), None, None);
        let runner_task = runner.spawn();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(reply) = ctx.drain_inbox(&controller).into_iter().next() {
                    break reply;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("controlled response must reach the controller");
        runner_task.abort();

        assert_eq!(reply.from, orchestrator);
        assert_eq!(reply.message_type, MessageType::Direct);
        assert_eq!(reply.task_id.as_deref(), Some(task_id));
        assert_eq!(reply.body, "ACTION: FINISH\nroger");
        // The initial plain ACTION must fail and be retried before the
        // correlated tool reply. The correlated valid ACTION is the decision
        // terminal: it seals the run, so the pre-seal follow-up text turn no
        // longer exists and exactly two provider calls remain.
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "initial plain ACTION must be retried once; the correlated ACTION \
             then seals the run (no follow-up provider turn)"
        );
    }

    #[tokio::test]
    async fn controlled_worker_plain_text_gets_same_session_finalization() {
        use llm_harness_agent::Plugin;
        use llm_harness_strategy::status_panel::TodoStore;

        let task_id = "task-pipe-issue-7-worker-1";
        let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
            llm_harness_loop::test_utils::MockResponse::text(
                "implementation complete but not reported",
            ),
            llm_harness_loop::test_utils::MockResponse::tool_use_end_turn(
                "reply-1",
                "send_message",
                &serde_json::json!({
                    "to": PIPELINE_CONTROLLER,
                    "task_id": task_id,
                    "task_kind": "result",
                    "body": "implementation and tests passed",
                })
                .to_string(),
            ),
            llm_harness_loop::test_utils::MockResponse::text("reply delivered"),
        ]));
        let ctx = SocialContext::new(10);
        let coder = AgentId::new("coder");
        let controller = AgentId::new(PIPELINE_CONTROLLER);
        ctx.register_agent(coder.clone(), "implementation");
        ctx.register_agent(controller.clone(), "controller");
        ctx.ensure_contact(&coder, &controller);

        let plugin = crate::SocialToolPlugin::new(coder.clone(), ctx.clone(), TodoStore::new());
        let mut options = llm_harness_agent::AgentHarnessOptions::new("test-model");
        plugin.register_tools(&mut options.tools);
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                client.clone() as Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                options,
            )
            .await,
        );
        ctx.send_direct_with_task(
            &controller,
            &coder,
            "implement and test",
            0,
            Some(task_id.into()),
            None,
        )
        .unwrap();

        let runner = AgentRunner::new(coder.clone(), harness, ctx.clone(), None, None);
        let runner_task = runner.spawn();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(reply) = ctx.drain_inbox(&controller).into_iter().next() {
                    break reply;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("terminal controlled worker result must reach the controller");
        runner_task.abort();

        assert_eq!(reply.from, coder);
        assert_eq!(reply.task_id.as_deref(), Some(task_id));
        assert_eq!(reply.body, "implementation and tests passed");
        assert_eq!(reply.task_kind, Some(crate::TaskMessageKind::Result));
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one work response plus the forced terminal-report turn must finish without a continuation or full task retry"
        );
        let requests = client.captured_requests.lock();
        assert!(
            format!("{:?}", requests[1].tool_choice()).contains("Specific(\"send_message\")"),
            "the corrective turn must force send_message"
        );
        assert!(
            format!("{:?}", requests[1].messages())
                .contains("implementation complete but not reported"),
            "the corrective turn must retain the work turn in the same session context"
        );
    }

    #[test]
    fn controlled_retry_categories_and_backoff_are_stable_and_observable() {
        assert_eq!(MAX_CONTROLLED_ATTEMPTS, 3);
        assert_eq!(
            ControlledFailureCategory::ProviderOrHarnessFailure.as_str(),
            "provider_or_harness_failure"
        );
        assert_eq!(
            ControlledFailureCategory::MissingTerminalReport.as_str(),
            "missing_terminal_report"
        );
        assert_eq!(retry_backoff_secs(1, 0), 1);
        assert_eq!(retry_backoff_secs(2, 0), 2);
        // The ordinary runner policy remains unchanged: failures 1 and 2 are
        // immediate, then 1s/2s/4s from failures 3/4/5.
        assert_eq!(retry_backoff_secs(3, 2), 1);
        assert_eq!(retry_backoff_secs(4, 2), 2);
        assert_eq!(retry_backoff_secs(5, 2), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn controlled_provider_failure_stops_after_three_attempts_and_quiesces() {
        let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        struct AlwaysFail(Arc<std::sync::atomic::AtomicU32>);

        #[async_trait::async_trait]
        impl llm_harness_types::BeforeRunHook for AlwaysFail {
            fn before_run<'a>(
                &'a self,
                _ctx: llm_harness_types::BeforeRunCtx<'a>,
            ) -> futures::future::BoxFuture<
                'a,
                Result<llm_harness_types::BeforeRunResult, llm_harness_types::AgentError>,
            > {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async {
                    Err(llm_harness_types::AgentError::Internal(
                        "deterministic harness failure".into(),
                    ))
                })
            }
        }

        let mut options = llm_harness_agent::AgentHarnessOptions::new("test-model");
        options
            .hooks
            .before_run
            .push(Arc::new(AlwaysFail(attempts.clone())));
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                options,
            )
            .await,
        );
        let ctx = SocialContext::new(10);
        let coder = AgentId::new("coder");
        let controller = AgentId::new(PIPELINE_CONTROLLER);
        ctx.register_agent(coder.clone(), "implementation");
        ctx.register_agent(controller.clone(), "controller");
        ctx.send_direct_with_task(
            &controller,
            &coder,
            "implement",
            0,
            Some("task-pipe-provider-failure".into()),
            None,
        )
        .unwrap();

        let runner_task = AgentRunner::new(coder.clone(), harness, ctx.clone(), None, None).spawn();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);

        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);

        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);

        let reply = ctx
            .drain_inbox(&controller)
            .into_iter()
            .next()
            .expect("retry exhaustion must deliver a correlated runtime error");
        assert_eq!(reply.task_kind, Some(crate::TaskMessageKind::Error));
        assert_eq!(reply.task_id.as_deref(), Some("task-pipe-provider-failure"));
        assert!(reply.body.contains("runtime execution failure"));
        assert!(reply.body.contains("provider_or_harness_failure"));

        tokio::time::advance(std::time::Duration::from_secs(120)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "a fourth controlled harness run must never occur"
        );
        assert_eq!(
            ctx.get_agent_info(&coder).unwrap().status,
            AgentStatus::Idle,
            "retry exhaustion must not leave the worker Busy"
        );
        assert!(!ctx.has_messages(&coder));
        runner_task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_during_controlled_backoff_prevents_the_next_attempt() {
        let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        struct AlwaysFail(Arc<std::sync::atomic::AtomicU32>);

        #[async_trait::async_trait]
        impl llm_harness_types::BeforeRunHook for AlwaysFail {
            fn before_run<'a>(
                &'a self,
                _ctx: llm_harness_types::BeforeRunCtx<'a>,
            ) -> futures::future::BoxFuture<
                'a,
                Result<llm_harness_types::BeforeRunResult, llm_harness_types::AgentError>,
            > {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async {
                    Err(llm_harness_types::AgentError::Internal(
                        "deterministic failure".into(),
                    ))
                })
            }
        }

        let mut options = llm_harness_agent::AgentHarnessOptions::new("test-model");
        options
            .hooks
            .before_run
            .push(Arc::new(AlwaysFail(attempts.clone())));
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                options,
            )
            .await,
        );
        let ctx = SocialContext::new(10);
        let coder = AgentId::new("coder");
        let controller = AgentId::new(PIPELINE_CONTROLLER);
        let task_id = "task-pipe-cancel-backoff";
        ctx.register_agent(coder.clone(), "implementation");
        ctx.register_agent(controller.clone(), "controller");
        ctx.begin_controlled_task(task_id);
        ctx.send_direct_with_task(
            &controller,
            &coder,
            "implement",
            0,
            Some(task_id.into()),
            None,
        )
        .unwrap();

        let runner = AgentRunner::new(coder.clone(), harness, ctx.clone(), None, None).spawn();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(ctx.cancel_controlled_task(task_id));
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a cancelled assignment must not start its scheduled retry"
        );
        assert_eq!(
            ctx.get_agent_info(&coder).unwrap().status,
            AgentStatus::Idle
        );
        assert!(!ctx.has_messages(&coder));
        runner.abort();
    }

    #[tokio::test]
    async fn cancelling_an_active_controlled_run_aborts_without_retry() {
        let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
            llm_harness_loop::test_utils::MockResponse::stall_after(1),
        ]));
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                client.clone() as Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                llm_harness_agent::AgentHarnessOptions::new("test-model"),
            )
            .await,
        );
        let ctx = SocialContext::new(10);
        let coder = AgentId::new("coder");
        let controller = AgentId::new(PIPELINE_CONTROLLER);
        let task_id = "task-pipe-cancel-active";
        ctx.register_agent(coder.clone(), "implementation");
        ctx.register_agent(controller.clone(), "controller");
        ctx.begin_controlled_task(task_id);
        ctx.send_direct_with_task(
            &controller,
            &coder,
            "implement",
            0,
            Some(task_id.into()),
            None,
        )
        .unwrap();

        let runner = AgentRunner::new(coder.clone(), harness, ctx.clone(), None, None).spawn();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while client.call_count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(ctx.cancel_controlled_task(task_id));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while ctx.get_agent_info(&coder).unwrap().status != AgentStatus::Idle {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled active worker must quiesce");
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(!ctx.has_messages(&coder));
        runner.abort();
    }

    #[tokio::test]
    async fn cancelling_an_active_orchestrator_decision_aborts_without_retry() {
        use llm_harness_agent::Plugin;
        use llm_harness_strategy::status_panel::TodoStore;

        let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
            llm_harness_loop::test_utils::MockResponse::stall_after(1),
        ]));
        let ctx = SocialContext::new(10);
        let orchestrator = AgentId::new(ORCHESTRATOR);
        let controller = AgentId::new(PIPELINE_CONTROLLER);
        let task_id = "task-pipe-orchestrator-cancel-active";
        ctx.register_agent(orchestrator.clone(), "decisions");
        ctx.register_agent(controller.clone(), "controller");
        ctx.ensure_contact(&orchestrator, &controller);
        let plugin =
            crate::SocialToolPlugin::new(orchestrator.clone(), ctx.clone(), TodoStore::new());
        let mut options = llm_harness_agent::AgentHarnessOptions::new("test-model");
        plugin.register_tools(&mut options.tools);
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                client.clone() as Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                options,
            )
            .await,
        );
        ctx.begin_controlled_task(task_id);
        ctx.send_direct_with_task(
            &controller,
            &orchestrator,
            "choose an action",
            0,
            Some(task_id.into()),
            None,
        )
        .unwrap();

        let runner =
            AgentRunner::new(orchestrator.clone(), harness, ctx.clone(), None, None).spawn();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while client.call_count.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(ctx.cancel_controlled_task(task_id));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while ctx.get_agent_info(&orchestrator).unwrap().status != AgentStatus::Idle {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled Orchestrator decision must quiesce");
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(!ctx.has_messages(&orchestrator));
        runner.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_orchestrator_backoff_does_not_start_another_attempt() {
        use llm_harness_agent::Plugin;
        use llm_harness_strategy::status_panel::TodoStore;

        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct AlwaysFail(Arc<std::sync::atomic::AtomicUsize>);

        #[async_trait::async_trait]
        impl llm_harness_types::BeforeRunHook for AlwaysFail {
            fn before_run<'a>(
                &'a self,
                _ctx: llm_harness_types::BeforeRunCtx<'a>,
            ) -> futures::future::BoxFuture<
                'a,
                Result<llm_harness_types::BeforeRunResult, llm_harness_types::AgentError>,
            > {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async {
                    tokio::task::yield_now().await;
                    Err(llm_harness_types::AgentError::Internal(
                        "decision provider failed".into(),
                    ))
                })
            }
        }

        let ctx = SocialContext::new(10);
        let orchestrator = AgentId::new(ORCHESTRATOR);
        let controller = AgentId::new(PIPELINE_CONTROLLER);
        let task_id = "task-pipe-orchestrator-cancel-backoff";
        ctx.register_agent(orchestrator.clone(), "decisions");
        ctx.register_agent(controller.clone(), "controller");
        ctx.ensure_contact(&orchestrator, &controller);
        let plugin =
            crate::SocialToolPlugin::new(orchestrator.clone(), ctx.clone(), TodoStore::new());
        let mut options = llm_harness_agent::AgentHarnessOptions::new("test-model");
        options
            .hooks
            .before_run
            .push(Arc::new(AlwaysFail(attempts.clone())));
        plugin.register_tools(&mut options.tools);
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                options,
            )
            .await,
        );
        ctx.begin_controlled_task(task_id);
        ctx.send_direct_with_task(
            &controller,
            &orchestrator,
            "choose an action",
            0,
            Some(task_id.into()),
            None,
        )
        .unwrap();
        let runner =
            AgentRunner::new(orchestrator.clone(), harness, ctx.clone(), None, None).spawn();

        for _ in 0..100 {
            if attempts.load(std::sync::atomic::Ordering::SeqCst) >= 3
                && ctx.has_messages(&orchestrator)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(ctx.cancel_controlled_task(task_id));
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "a cancelled decision must not start its scheduled retry"
        );
        assert_eq!(
            ctx.get_agent_info(&orchestrator).unwrap().status,
            AgentStatus::Idle
        );
        assert!(!ctx.has_messages(&orchestrator));
        runner.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn controlled_missing_terminal_uses_the_same_three_attempt_budget() {
        let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
            llm_harness_loop::test_utils::MockResponse::text("attempt one ended without report"),
            llm_harness_loop::test_utils::MockResponse::text("attempt one finalization failed"),
            llm_harness_loop::test_utils::MockResponse::text("attempt two ended without report"),
            llm_harness_loop::test_utils::MockResponse::text("attempt two finalization failed"),
            llm_harness_loop::test_utils::MockResponse::text("attempt three ended without report"),
            llm_harness_loop::test_utils::MockResponse::text("attempt three finalization failed"),
        ]));
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                client.clone() as Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                llm_harness_agent::AgentHarnessOptions::new("test-model"),
            )
            .await,
        );
        let ctx = SocialContext::new(10);
        let coder = AgentId::new("coder");
        let controller = AgentId::new(PIPELINE_CONTROLLER);
        ctx.register_agent(coder.clone(), "implementation");
        ctx.register_agent(controller.clone(), "controller");
        ctx.send_direct_with_task(
            &controller,
            &coder,
            "implement",
            0,
            Some("task-pipe-missing-terminal".into()),
            None,
        )
        .unwrap();

        let runner_task = AgentRunner::new(coder.clone(), harness, ctx.clone(), None, None).spawn();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            3
        );
        let reply = ctx
            .drain_inbox(&controller)
            .into_iter()
            .next()
            .expect("missing terminal exhaustion must reach the controller");
        assert_eq!(reply.task_kind, Some(crate::TaskMessageKind::Error));
        assert!(reply.body.contains("missing_terminal_report"));
        assert_eq!(
            ctx.get_agent_info(&coder).unwrap().status,
            AgentStatus::Idle
        );
        assert!(!ctx.has_messages(&coder));
        runner_task.abort();
    }

    #[tokio::test]
    async fn every_typed_terminal_worker_outcome_stops_retrying_and_is_preserved() {
        use llm_harness_agent::Plugin;
        use llm_harness_strategy::status_panel::TodoStore;

        for (member, suffix, kind) in [
            ("coder-result", "result", crate::TaskMessageKind::Result),
            ("reviewer", "review-result", crate::TaskMessageKind::Result),
            ("coder-blocked", "blocked", crate::TaskMessageKind::Blocked),
            ("coder-error", "error", crate::TaskMessageKind::Error),
        ] {
            let task_id = format!("task-pipe-terminal-{suffix}");
            let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
                llm_harness_loop::test_utils::MockResponse::tool_use_end_turn(
                    "terminal-reply",
                    "send_message",
                    &serde_json::json!({
                        "to": PIPELINE_CONTROLLER,
                        "task_id": task_id,
                        "task_kind": kind.as_str(),
                        "body": format!("{suffix} evidence"),
                    })
                    .to_string(),
                ),
                llm_harness_loop::test_utils::MockResponse::text(
                    "must never continue after terminal delivery",
                ),
            ]));
            let ctx = SocialContext::new(10);
            let worker = AgentId::new(member);
            let controller = AgentId::new(PIPELINE_CONTROLLER);
            ctx.register_agent(worker.clone(), "controlled worker");
            ctx.register_agent(controller.clone(), "controller");
            ctx.ensure_contact(&worker, &controller);

            let plugin =
                crate::SocialToolPlugin::new(worker.clone(), ctx.clone(), TodoStore::new());
            let mut options = llm_harness_agent::AgentHarnessOptions::new("test-model");
            plugin.register_tools(&mut options.tools);
            let harness = Arc::new(
                AgentHarness::new_in_memory(
                    client.clone() as Arc<dyn llm_harness_loop::LlmClient>,
                    Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                    options,
                )
                .await,
            );
            ctx.send_direct_with_task(
                &controller,
                &worker,
                "perform work",
                0,
                Some(task_id.clone()),
                None,
            )
            .unwrap();

            let runner_task =
                AgentRunner::new(worker.clone(), harness, ctx.clone(), None, None).spawn();
            let reply = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if let Some(reply) = ctx.drain_inbox(&controller).into_iter().next() {
                        break reply;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("typed terminal must reach controller");
            assert_eq!(reply.task_id.as_deref(), Some(task_id.as_str()));
            assert_eq!(reply.task_kind, Some(kind));
            assert_eq!(reply.body, format!("{suffix} evidence"));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(
                client.call_count.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "{suffix} must not start a post-terminal provider turn"
            );
            assert_eq!(
                ctx.get_agent_info(&worker).unwrap().status,
                AgentStatus::Idle
            );
            runner_task.abort();
        }
    }

    #[tokio::test]
    async fn terminal_delivery_aborts_a_lingering_coder_and_reviewer_run() {
        use llm_harness_agent::Plugin;
        use llm_harness_strategy::status_panel::TodoStore;

        struct LingerAfterSend(Arc<AtomicBool>);

        impl llm_harness_types::AfterToolCallHook for LingerAfterSend {
            fn on_complete<'a>(
                &'a self,
                ctx: llm_harness_types::AfterToolCallCtx<'a>,
            ) -> futures::future::BoxFuture<'a, llm_harness_types::AfterToolCallDecision>
            {
                Box::pin(async move {
                    if ctx.tool_name == "send_message" {
                        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                        std::future::pending::<()>().await;
                    }
                    llm_harness_types::AfterToolCallDecision::Passthrough
                })
            }
        }

        for member in ["coder", "reviewer"] {
            let task_id = format!("task-pipe-lingering-{member}");
            let entered_linger = Arc::new(AtomicBool::new(false));
            let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
                llm_harness_loop::test_utils::MockResponse::tool_use_end_turn(
                    "terminal-reply",
                    "send_message",
                    &serde_json::json!({
                        "to": PIPELINE_CONTROLLER,
                        "task_id": task_id,
                        "task_kind": "result",
                        "body": format!("{member} evidence"),
                    })
                    .to_string(),
                ),
            ]));
            let ctx = SocialContext::new(10);
            let worker = AgentId::new(member);
            let controller = AgentId::new(PIPELINE_CONTROLLER);
            ctx.register_agent(worker.clone(), "controlled worker");
            ctx.register_agent(controller.clone(), "controller");
            ctx.ensure_contact(&worker, &controller);

            let plugin =
                crate::SocialToolPlugin::new(worker.clone(), ctx.clone(), TodoStore::new());
            let mut options = llm_harness_agent::AgentHarnessOptions::new("test-model");
            options
                .hooks
                .after_tool_call
                .push(Arc::new(LingerAfterSend(entered_linger.clone())));
            plugin.register_tools(&mut options.tools);
            let harness = Arc::new(
                AgentHarness::new_in_memory(
                    client as Arc<dyn llm_harness_loop::LlmClient>,
                    Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                    options,
                )
                .await,
            );
            ctx.send_direct_with_task(
                &controller,
                &worker,
                "perform work",
                0,
                Some(task_id.clone()),
                None,
            )
            .unwrap();

            let runner_task =
                AgentRunner::new(worker.clone(), harness, ctx.clone(), None, None).spawn();
            let reply = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if let Some(reply) = ctx.drain_inbox(&controller).into_iter().next() {
                        break reply;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("terminal message must persist before the lingering run is aborted");
            assert_eq!(reply.task_kind, Some(crate::TaskMessageKind::Result));
            assert_eq!(reply.body, format!("{member} evidence"));
            assert!(entered_linger.load(std::sync::atomic::Ordering::SeqCst));

            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while ctx.get_agent_info(&worker).unwrap().status != AgentStatus::Idle {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("sealed terminal worker must quiesce promptly");
            runner_task.abort();
        }
    }

    /// Regression for the real 13:50:59Z failure: a valid Orchestrator ACTION
    /// was durably delivered but the harness kept running, so the member
    /// stayed Busy past the controller's 20s quiescence budget and the whole
    /// workflow died with "runner orchestrator did not finish after its
    /// reply". The decision-local seal must abort the lingering run and let
    /// the real runner loop reach Idle.
    #[tokio::test]
    async fn decision_delivery_aborts_a_lingering_orchestrator_run() {
        use llm_harness_agent::Plugin;
        use llm_harness_strategy::status_panel::TodoStore;

        struct LingerAfterSend(Arc<AtomicBool>);

        impl llm_harness_types::AfterToolCallHook for LingerAfterSend {
            fn on_complete<'a>(
                &'a self,
                ctx: llm_harness_types::AfterToolCallCtx<'a>,
            ) -> futures::future::BoxFuture<'a, llm_harness_types::AfterToolCallDecision>
            {
                Box::pin(async move {
                    if ctx.tool_name == "send_message" {
                        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                        std::future::pending::<()>().await;
                    }
                    llm_harness_types::AfterToolCallDecision::Passthrough
                })
            }
        }

        let task_id = "task-pipe-lingering-decision-1";
        let entered_linger = Arc::new(AtomicBool::new(false));
        let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
            llm_harness_loop::test_utils::MockResponse::tool_use_end_turn(
                "decision-reply",
                "send_message",
                &serde_json::json!({
                    "to": PIPELINE_CONTROLLER,
                    "task_id": task_id,
                    "body": "ACTION: FINISH\nwork complete for the operator",
                })
                .to_string(),
            ),
        ]));
        let ctx = SocialContext::new(10);
        let orchestrator = AgentId::new(ORCHESTRATOR);
        let controller = AgentId::new(PIPELINE_CONTROLLER);
        ctx.register_agent(orchestrator.clone(), "workflow routing");
        ctx.register_agent(controller.clone(), "controller");
        ctx.ensure_contact(&orchestrator, &controller);

        let plugin =
            crate::SocialToolPlugin::new(orchestrator.clone(), ctx.clone(), TodoStore::new());
        let mut options = llm_harness_agent::AgentHarnessOptions::new("test-model");
        options
            .hooks
            .after_tool_call
            .push(Arc::new(LingerAfterSend(entered_linger.clone())));
        plugin.register_tools(&mut options.tools);
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                client as Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                options,
            )
            .await,
        );
        ctx.send_direct_with_task(
            &controller,
            &orchestrator,
            "choose the next action",
            0,
            Some(task_id.to_owned()),
            None,
        )
        .unwrap();

        let runner_task =
            AgentRunner::new(orchestrator.clone(), harness, ctx.clone(), None, None).spawn();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(reply) = ctx.drain_inbox(&controller).into_iter().next() {
                    break reply;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("decision message must persist before the lingering run is aborted");
        assert_eq!(reply.body, "ACTION: FINISH\nwork complete for the operator");
        assert!(entered_linger.load(std::sync::atomic::Ordering::SeqCst));

        // The real runner loop (not a mock) must observe the decision seal,
        // exit the aborted harness run, and transition Busy -> Idle well
        // inside the 20s controller quiescence budget.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while ctx.get_agent_info(&orchestrator).unwrap().status != AgentStatus::Idle {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("sealed decision run must quiesce promptly");
        runner_task.abort();
    }

    /// Regression for the real 12:38:47Z -> 12:38:51Z sequence: after the
    /// correlated DISPATCH decision was delivered, the same harness run
    /// continued and emitted a second FINISH ACTION for the same decision
    /// task. With the decision seal, the continuation provider turn and the
    /// second send_message can never execute.
    #[tokio::test]
    async fn decision_seal_prevents_a_second_action_after_dispatch() {
        use llm_harness_agent::Plugin;
        use llm_harness_strategy::status_panel::TodoStore;

        let task_id = "task-pipe-second-action-decision-1";
        let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
            llm_harness_loop::test_utils::MockResponse::tool_use_end_turn(
                "dispatch-reply",
                "send_message",
                &serde_json::json!({
                    "to": PIPELINE_CONTROLLER,
                    "task_id": task_id,
                    "body": "ACTION: DISPATCH coder\nimplement the assignment",
                })
                .to_string(),
            ),
            // The continuation would attempt a second ACTION.
            llm_harness_loop::test_utils::MockResponse::tool_use_end_turn(
                "late-finish",
                "send_message",
                &serde_json::json!({
                    "to": PIPELINE_CONTROLLER,
                    "task_id": task_id,
                    "body": "ACTION: FINISH\nunwanted second action",
                })
                .to_string(),
            ),
        ]));
        let ctx = SocialContext::new(10);
        let orchestrator = AgentId::new(ORCHESTRATOR);
        let controller = AgentId::new(PIPELINE_CONTROLLER);
        ctx.register_agent(orchestrator.clone(), "workflow routing");
        ctx.register_agent(controller.clone(), "controller");
        ctx.ensure_contact(&orchestrator, &controller);

        let plugin =
            crate::SocialToolPlugin::new(orchestrator.clone(), ctx.clone(), TodoStore::new());
        let mut options = llm_harness_agent::AgentHarnessOptions::new("test-model");
        plugin.register_tools(&mut options.tools);
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                client.clone() as Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                options,
            )
            .await,
        );
        ctx.send_direct_with_task(
            &controller,
            &orchestrator,
            "choose the next action",
            0,
            Some(task_id.to_owned()),
            None,
        )
        .unwrap();

        let runner_task =
            AgentRunner::new(orchestrator.clone(), harness, ctx.clone(), None, None).spawn();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(reply) = ctx.drain_inbox(&controller).into_iter().next() {
                    break reply;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first decision must reach the controller");
        assert_eq!(reply.body, "ACTION: DISPATCH coder\nimplement the assignment");

        // Wait for quiescence through the real runner loop, then prove the
        // sealed run never produced the second ACTION.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while ctx.get_agent_info(&orchestrator).unwrap().status != AgentStatus::Idle {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("sealed decision run must quiesce promptly");
        tokio::task::yield_now().await;
        assert_eq!(
            ctx.drain_inbox(&controller).len(),
            0,
            "the second ACTION must never be persisted as a side effect"
        );
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the DISPATCH decision seals the run: no continuation provider turn"
        );
        runner_task.abort();
    }

    /// A malformed correlated reply must not become a decision terminal: the
    /// run still needs the required tool, and the existing failure path
    /// (requeue) keeps the assignment observable.
    #[tokio::test(start_paused = true)]
    async fn malformed_orchestrator_reply_is_not_a_decision_terminal() {
        use llm_harness_agent::Plugin;
        use llm_harness_strategy::status_panel::TodoStore;

        let task_id = "task-pipe-malformed-decision-1";
        let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
            // Attempt 1: correlated send_message with a malformed first line.
            llm_harness_loop::test_utils::MockResponse::tool_use_end_turn(
                "bad-reply",
                "send_message",
                &serde_json::json!({
                    "to": PIPELINE_CONTROLLER,
                    "task_id": task_id,
                    "body": "prose first line\nACTION: FINISH\nlate",
                })
                .to_string(),
            ),
            // Attempt 2: a valid ACTION; the controller grammar stays the
            // semantic source of truth.
            llm_harness_loop::test_utils::MockResponse::tool_use_end_turn(
                "good-reply",
                "send_message",
                &serde_json::json!({
                    "to": PIPELINE_CONTROLLER,
                    "task_id": task_id,
                    "body": "ACTION: FINISH\nvalid retry after malformed prose",
                })
                .to_string(),
            ),
        ]));
        let ctx = SocialContext::new(10);
        let orchestrator = AgentId::new(ORCHESTRATOR);
        let controller = AgentId::new(PIPELINE_CONTROLLER);
        ctx.register_agent(orchestrator.clone(), "workflow routing");
        ctx.register_agent(controller.clone(), "controller");
        ctx.ensure_contact(&orchestrator, &controller);

        let plugin =
            crate::SocialToolPlugin::new(orchestrator.clone(), ctx.clone(), TodoStore::new());
        let mut options = llm_harness_agent::AgentHarnessOptions::new("test-model");
        plugin.register_tools(&mut options.tools);
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                client.clone() as Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                options,
            )
            .await,
        );
        ctx.send_direct_with_task(
            &controller,
            &orchestrator,
            "choose the next action",
            0,
            Some(task_id.to_owned()),
            None,
        )
        .unwrap();

        let runner_task =
            AgentRunner::new(orchestrator.clone(), harness, ctx.clone(), None, None).spawn();
        // The malformed reply reaches the controller inbox as an ordinary
        // message (it is not a decision terminal, so the run does not seal),
        // but the runner must NOT treat it as terminal success: the
        // assignment is requeued and attempt 2 supplies a valid ACTION.
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let messages = ctx.drain_inbox(&controller);
                if let Some(reply) = messages
                    .into_iter()
                    .find(|m| m.body.starts_with("ACTION:"))
                {
                    break reply;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("valid ACTION retry must reach the controller");
        assert_eq!(reply.body, "ACTION: FINISH\nvalid retry after malformed prose");
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "malformed reply must not be a terminal: the runner requeues and \
             the retry delivers a valid ACTION"
        );
        runner_task.abort();
    }

    #[tokio::test]
    async fn terminal_result_prevents_a_later_mutating_tool_turn() {
        use llm_harness_agent::Plugin;
        use llm_harness_strategy::status_panel::TodoStore;
        use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

        struct MutatingTool(Arc<std::sync::atomic::AtomicUsize>);

        impl Tool for MutatingTool {
            fn name(&self) -> &str {
                "mutate_workspace"
            }

            fn description(&self) -> &str {
                "test-only mutating tool"
            }

            fn parameters_schema(&self) -> &serde_json::Value {
                static SCHEMA: std::sync::LazyLock<serde_json::Value> =
                    std::sync::LazyLock::new(|| serde_json::json!({"type": "object"}));
                &SCHEMA
            }

            fn execute<'a>(
                &'a self,
                _args: serde_json::Value,
                _ctx: &'a ToolContext,
            ) -> futures::future::BoxFuture<'a, Result<ToolResult, ToolFailure>> {
                Box::pin(async move {
                    self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(ToolResult::full(
                        vec![DataBlock::text("mutated")],
                        serde_json::Value::Null,
                        false,
                    ))
                })
            }
        }

        let task_id = "task-pipe-no-post-terminal-mutation";
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
            llm_harness_loop::test_utils::MockResponse::tool_use_end_turn(
                "terminal-reply",
                "send_message",
                &serde_json::json!({
                    "to": PIPELINE_CONTROLLER,
                    "task_id": task_id,
                    "task_kind": "result",
                    "body": "workspace complete",
                })
                .to_string(),
            ),
            llm_harness_loop::test_utils::MockResponse::tool_use_end_turn(
                "late-mutation",
                "mutate_workspace",
                "{}",
            ),
        ]));
        let ctx = SocialContext::new(10);
        let coder = AgentId::new("coder");
        let controller = AgentId::new(PIPELINE_CONTROLLER);
        ctx.register_agent(coder.clone(), "implementation");
        ctx.register_agent(controller.clone(), "controller");
        ctx.ensure_contact(&coder, &controller);

        let plugin = crate::SocialToolPlugin::new(coder.clone(), ctx.clone(), TodoStore::new());
        let mut options = llm_harness_agent::AgentHarnessOptions::new("test-model");
        plugin.register_tools(&mut options.tools);
        options
            .tools
            .push(Arc::new(MutatingTool(mutations.clone())));
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                client.clone() as Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                options,
            )
            .await,
        );
        ctx.send_direct_with_task(
            &controller,
            &coder,
            "perform work",
            0,
            Some(task_id.into()),
            None,
        )
        .unwrap();

        let runner_task = AgentRunner::new(coder.clone(), harness, ctx.clone(), None, None).spawn();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if ctx.get_agent_info(&coder).unwrap().status == AgentStatus::Idle
                    && !ctx.drain_inbox(&controller).is_empty()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("terminal worker must deliver and quiesce");

        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "terminal delivery must prevent a later provider turn"
        );
        assert_eq!(
            mutations.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "post-terminal mutating tool must not execute"
        );
        runner_task.abort();
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
            task_kind: None,
            broadcast_id: None,
            message_id: None,
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
            task_kind: None,
            broadcast_id: None,
            message_id: None,
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
            task_kind: None,
            broadcast_id: None,
            message_id: None,
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
            task_kind: None,
            broadcast_id: None,
            message_id: None,
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
            task_kind: None,
            broadcast_id: None,
            message_id: None,
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
            task_kind: None,
            broadcast_id: None,
            message_id: None,
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

    /// Regression (lost-steer follow-up, Agent-Team-hosted execution): when a
    /// steer the loop never consumed is salvaged at teardown, the source
    /// InboxMessage must reach the model EXACTLY ONCE MORE — via the
    /// harness's own buffered salvage (next-turn messages) consumed by a
    /// continuation run — and never via a social-inbox requeue.
    ///
    /// Before the fix the runner requeued the drained batch on salvage; the
    /// batch was then ALSO acked/returned by the disposition branch, so the
    /// same message could be delivered twice (double requeue) or acked while
    /// still pending redelivery. The continuation-run design keeps the
    /// journal row intact until the buffered salvage has actually been
    /// consumed (verified consumption boundary), and a salvage-triggered
    /// batch is acked at most once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn salvaged_steer_is_delivered_once_by_continuation_not_requeue() {
        use std::sync::OnceLock;

        // The hook needs the harness before it exists; share it via OnceLock.
        static HARNESS: OnceLock<Arc<AgentHarness>> = OnceLock::new();
        // One-shot: only the FIRST run's final stop decision steers. A steer
        // on the continuation run would re-salvage and never quiesce — an
        // artifact of the test, not the production race.
        static STEERED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);

        let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
            llm_harness_loop::test_utils::MockResponse::text("first run done"),
        ]));
        let mut opts = llm_harness_agent::AgentHarnessOptions::new("test-model");
        struct SteerFromOnceLock(&'static str);
        impl llm_harness_types::ShouldStopHook for SteerFromOnceLock {
            fn should_stop<'a>(
                &'a self,
                _ctx: llm_harness_types::ShouldStopCtx<'a>,
            ) -> futures::future::BoxFuture<'a, bool> {
                Box::pin(async move {
                    if !STEERED.swap(true, std::sync::atomic::Ordering::SeqCst)
                        && let Some(harness) = HARNESS.get()
                    {
                        let _ = harness.steer_message(
                            llm_harness_types::AgentMessage::User(
                                llm_harness_types::UserMessage {
                                    content: vec![llm_harness_types::ContentBlock::Text {
                                        text: self.0.to_owned(),
                                    }],
                                    timestamp: chrono::Utc::now(),
                                },
                            ),
                        );
                    }
                    true
                })
            }
        }
        opts.hooks
            .should_stop
            .push(Arc::new(SteerFromOnceLock("late steered instruction")));
        let harness = Arc::new(
            AgentHarness::new_in_memory(
                client.clone() as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
                Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                opts,
            )
            .await,
        );
        if HARNESS.set(harness.clone()).is_err() {
            panic!("harness set once");
        }
        let _ = client; // kept alive for call_count assertions below
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "seed task".into(),
            message_type: MessageType::Direct,
            priority: 0,
            task_id: None,
            task_kind: None,
            broadcast_id: None,
            message_id: None,
        });

        let runner = AgentRunner::new(alice.clone(), harness.clone(), ctx.clone(), None, None);
        let task = runner.spawn();

        // Bounded settle: run 1 finishes with the salvage buffered; the
        // runner's continuation run consumes it; the batch is acked once.
        for _ in 0..300 {
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
            "salvage continuation must settle the agent Idle"
        );
        assert!(
            !ctx.has_messages(&alice),
            "salvaged source message must NOT be requeued into the social inbox"
        );
        assert!(
            !harness.has_queued_messages(),
            "harness salvage buffer must be empty after the continuation run consumed it"
        );

        // Delivery-once proof from the model's actual requests: the salvaged
        // text appears in exactly ONE provider request beyond run 1's. A
        // requeue-based double delivery would inject it into TWO runs (and a
        // lost salvage into none).
        let captured = client.captured_requests.lock();
        let salvage_deliveries = captured
            .iter()
            .filter(|req| {
                req.messages().iter().any(|m| {
                    matches!(m, llm_harness_loop::LlmMessage::User(content)
                        if content.iter().any(|c| matches!(c,
                            llm_harness_loop::RequestContent::Text(text)
                                if text.contains("late steered instruction"))))
                })
            })
            .count();
        assert_eq!(
            salvage_deliveries, 1,
            "salvaged steer must be model-visible exactly once across all runs"
        );

        task.abort();
    }

    /// Regression (durable ACK ownership, lost-steer follow-up): a salvaged
    /// message's journal row must stay pending until the salvage has actually
    /// been consumed by a run (verified consumption boundary), and the ack
    /// must remove the row EXACTLY ONCE — no double requeue may resurrect it,
    /// and a crash before consumption must still find the row recoverable.
    ///
    /// Sequence proven here through the REAL journal:
    ///   1. push → row exists, message drained (row still present, in flight).
    ///   2. salvage at run teardown → source returned once, row STILL present
    ///      (durability preserved: crash here redelivers rather than loses).
    ///   3. continuation consumed the salvage → ack → row gone exactly once;
    ///      a second ack is a no-op (not an error, not a resurrect).
    #[tokio::test]
    async fn salvaged_message_journal_row_survives_until_consumption_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            crate::inbox_journal::InboxJournal::open(temp.path().join("inbox.sqlite3")).unwrap(),
        );
        let ctx = SocialContext::with_journal(10, Some(journal.clone()), None).unwrap();
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        // 1. Durable push: row exists before the in-memory queue accepts it.
        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new("bob"),
            to: alice.clone(),
            body: "work to do".into(),
            message_type: MessageType::Direct,
            priority: 0,
            task_id: None,
            task_kind: None,
            broadcast_id: None,
            message_id: None,
        })
        .unwrap();
        assert_eq!(journal.pending_count().unwrap(), 1, "push must journal");

        // Runner drains the batch (in-flight consumption: row still pending).
        let messages = ctx.drain_inbox(&alice);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            journal.pending_count().unwrap(),
            1,
            "drain alone must not ack: a crash here must redeliver"
        );

        // 2. Salvage: the runner requeues the batch exactly once (the fixed
        // single-disposition path). The durable row must STILL be pending —
        // the requeued copy is volatile, so deleting the row now would open a
        // crash-loss window.
        ctx.return_messages(&alice, messages.clone());
        assert_eq!(
            journal.pending_count().unwrap(),
            1,
            "salvage requeue must NOT delete the durable row: recovery needs it"
        );
        // Idempotent single-owner requeue: returning twice must not create a
        // second journal row (return_messages never journals).
        ctx.return_messages(&alice, messages.clone());
        assert_eq!(
            journal.pending_count().unwrap(),
            1,
            "double return must not duplicate the journal row"
        );

        // 3. Verified consumption boundary: the continuation run consumed the
        // buffered salvage. Ack once — row gone; ack again — no resurrection.
        ctx.ack_inbox_messages(&messages);
        assert_eq!(
            journal.pending_count().unwrap(),
            0,
            "ack at the consumption boundary must remove the durable row"
        );
        ctx.ack_inbox_messages(&messages);
        assert_eq!(
            journal.pending_count().unwrap(),
            0,
            "a second ack must be a no-op, never resurrect the row"
        );
    }

    /// END-TO-END ownership experiment (evidence only, no production change):
    /// a REAL journal-backed InboxMessage is drained by the PRODUCTION
    /// monitor task during an active run, steered through the real
    /// `steer_message` path, salvaged at run teardown, and disposed of by
    /// the actual Runner disposition branches — observed through the
    /// durable journal, captured provider requests, the session transcript,
    /// and agent status. Count only the marker's first appearance in a
    /// provider request; later appearances may be conversation-history replay.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[deny(unused_must_use)]
    async fn monitor_drained_durable_message_delivery_ack_recovery_e2e() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        /// Parks the FIRST stop decision of the FIRST run so the test can
        /// inject a mid-run message deterministically; releases on demand.
        struct ParkGate {
            parked: Arc<AtomicBool>,
            release: Arc<tokio::sync::Notify>,
            armed: AtomicBool,
        }
        impl llm_harness_types::ShouldStopHook for ParkGate {
            fn should_stop<'a>(
                &'a self,
                _ctx: llm_harness_types::ShouldStopCtx<'a>,
            ) -> futures::future::BoxFuture<'a, bool> {
                let parked = Arc::clone(&self.parked);
                let release = Arc::clone(&self.release);
                Box::pin(async move {
                    if self.armed.swap(false, Ordering::SeqCst) {
                        parked.store(true, Ordering::SeqCst);
                        release.notified().await;
                    }
                    true
                })
            }
        }

        async fn wait_until(mut probe: impl FnMut() -> bool, what: &str) {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
            while !probe() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for {what}"
                );
                tokio::task::yield_now().await;
            }
        }

        fn new_delivery_count(requests: &[llm_harness_loop::ChatRequest], marker: &str) -> usize {
            let mut seen_before = false;
            requests
                .iter()
                .filter(|request| {
                    let present = request.messages().iter().any(|message| {
                        matches!(message, llm_harness_loop::LlmMessage::User(content)
                        if content.iter().any(|part| matches!(part,
                            llm_harness_loop::RequestContent::Text(text)
                                if text.contains(marker))))
                    });
                    let newly_introduced = present && !seen_before;
                    seen_before |= present;
                    newly_introduced
                })
                .count()
        }

        // Shared per-scenario handles so the test body can park/release.
        struct Scenario {
            _temp: tempfile::TempDir,
            journal: Arc<crate::inbox_journal::InboxJournal>,
            client: Arc<llm_harness_loop::test_utils::MockLlmClient>,
            harness: Arc<AgentHarness>,
            ctx: SocialContext,
            alice: AgentId,
            parked: Arc<AtomicBool>,
            release: Arc<tokio::sync::Notify>,
            continuation: Arc<ContinuationProbe>,
            events: tokio::sync::broadcast::Receiver<Arc<llm_harness_agent::AgentHarnessEvent>>,
            runner: tokio::task::JoinHandle<()>,
        }

        async fn spawn_scenario(continuation_fails: bool) -> Scenario {
            let temp = tempfile::tempdir().unwrap();
            let journal = Arc::new(
                crate::inbox_journal::InboxJournal::open(temp.path().join("inbox.sqlite3"))
                    .unwrap(),
            );
            let mut presets = vec![llm_harness_loop::test_utils::MockResponse::text(
                "run one done",
            )];
            if continuation_fails {
                presets.push(llm_harness_loop::test_utils::MockResponse::rate_limit_error());
            }
            presets.push(llm_harness_loop::test_utils::MockResponse::text(
                "salvage consumed",
            ));
            let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(presets));
            let mut opts = llm_harness_agent::AgentHarnessOptions::new("test-model");
            let parked = Arc::new(AtomicBool::new(false));
            let release = Arc::new(tokio::sync::Notify::new());
            opts.hooks.should_stop.push(Arc::new(ParkGate {
                parked: Arc::clone(&parked),
                release: Arc::clone(&release),
                armed: AtomicBool::new(true),
            }));
            let harness = Arc::new(
                AgentHarness::new_in_memory(
                    client.clone() as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
                    Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                    opts,
                )
                .await,
            );
            let ctx = SocialContext::with_journal(10, Some(journal.clone()), None).unwrap();
            let alice = AgentId::new("alice");
            ctx.register_agent(alice.clone(), "rust");
            let continuation = ContinuationProbe::new();
            let events = harness.subscribe();
            let mut runner =
                AgentRunner::new(alice.clone(), harness.clone(), ctx.clone(), None, None);
            runner.continuation_probe = Some(continuation.clone());
            let runner = runner.spawn();
            Scenario {
                _temp: temp,
                journal,
                client,
                harness,
                ctx,
                alice,
                parked,
                release,
                continuation,
                events,
                runner,
            }
        }

        let bob = AgentId::new("bob");
        let push = |scenario: &Scenario, body: &str| {
            scenario
                .ctx
                .push_message(InboxMessage {
                    journal_id: None,
                    from: bob.clone(),
                    to: scenario.alice.clone(),
                    body: body.into(),
                    message_type: MessageType::Direct,
                    priority: 0,
                    task_id: None,
                    task_kind: None,
                    broadcast_id: None,
                    message_id: None,
                })
                .unwrap();
        };

        // ── Scenario 1: salvage; continuation run SUCCEEDS ──────────────────
        const MARK_OK: &str = "MONITOR-SALVAGE-OK-7f3a";
        {
            let mut s = spawn_scenario(false).await;
            let outcome = std::panic::AssertUnwindSafe(async {
                // 1. Durable batch message A → row r1; the runner's loop-top
                //    drain consumes it into run 1.
                push(&s, "batch work for alice");
                let r1 = s.journal.recover().unwrap().last().unwrap().id;
                assert_eq!(s.journal.pending_count().unwrap(), 1, "push must journal");

                wait_until(
                    || s.client.call_count.load(Ordering::SeqCst) == 1,
                    "run 1 to issue its provider request",
                )
                .await;
                wait_until(
                    || s.parked.load(Ordering::SeqCst),
                    "run 1 to park at its final stop decision",
                )
                .await;
                assert_eq!(
                    s.journal.pending_count().unwrap(),
                    1,
                    "drain alone must not ack: in-flight row stays pending"
                );

                // 2. Mid-run durable message B → row r2. The PRODUCTION monitor
                //    drains it during the live run and steers via the real path.
                push(&s, MARK_OK);
                let r2 = s.journal.recover().unwrap().last().unwrap().id;
                assert_ne!(r1, r2, "distinct durable rows");
                wait_until(
                    || !s.ctx.has_messages(&s.alice),
                    "production monitor to drain the mid-run message",
                )
                .await;
                wait_until(
                    || s.harness.has_queued_messages(),
                    "production monitor to complete steering into the live channel",
                )
                .await;
                assert!(
                    s.parked.load(Ordering::SeqCst),
                    "the run must still be parked when steering completes"
                );
                assert_eq!(
                    s.ctx.get_agent_info(&s.alice).unwrap().status,
                    AgentStatus::Busy,
                    "the runner must be Busy while run 1 is parked"
                );
                assert_eq!(
                    s.journal.pending_count().unwrap(),
                    2,
                    "steered message's row must stay pending while buffered"
                );

                // 3. Release: run 1 exits WITHOUT consuming the steer; salvage
                //    buffers it; the runner's continuation run delivers it.
                s.release.notify_one();
                wait_until(
                    || s.continuation.snapshot().disposition_reached,
                    "runner to reach run-1 disposition",
                )
                .await;
                wait_until(
                    || s.continuation.snapshot().result.is_some(),
                    "runner continuation to return",
                )
                .await;
                wait_until(
                    || {
                        s.ctx.get_agent_info(&s.alice).map(|i| i.status) == Some(AgentStatus::Idle)
                            && !s.ctx.has_messages(&s.alice)
                    },
                    "runner to settle Idle after the continuation",
                )
                .await;
                let continuation = s.continuation.snapshot();
                assert_eq!(continuation.salvage_flag, Some(true));
                assert_eq!(continuation.queued_at_disposition, Some(true));
                assert!(continuation.entered);
                assert_eq!(continuation.result.as_deref(), Some("Ok(())"));
                assert_eq!(s.client.call_count.load(Ordering::SeqCst), 2);
                assert_eq!(
                    s.harness.state().phase,
                    llm_harness_types::HarnessPhase::Idle
                );
                assert!(s.harness.state().error_message.is_none());
                let mut orphaned_r2 = false;
                while let Ok(event) = s.events.try_recv() {
                    if let llm_harness_agent::AgentHarnessEvent::Agent(
                        llm_harness_types::AgentEvent::SteerOrphaned { messages },
                    ) = event.as_ref()
                    {
                        orphaned_r2 |= messages.iter().any(|message| {
                            matches!(message,
                        AgentMessage::User(user) if user.content.iter().any(|part| matches!(part,
                            ContentBlock::Text { text } if text.contains(MARK_OK))))
                        });
                    }
                }
                assert!(orphaned_r2, "run 1 must hand r2 into harness salvage");
                assert!(
                    !s.harness.has_queued_messages(),
                    "continuation must empty harness buffer and channels"
                );

                // Evidence: model-visible delivery (fresh user turn contains the
                // marker), persisted in the session, NOT requeued to the inbox.
                let deliveries = new_delivery_count(&s.client.captured_requests.lock(), MARK_OK);
                assert_eq!(
                    deliveries, 1,
                    "monitor-drained message must be model-visible exactly once"
                );
                let context = s.harness.build_context().await.unwrap();
                assert!(
                    context.messages.iter().any(|m| matches!(
                        m,
                        AgentMessage::User(u) if u.content.iter().any(|c| matches!(
                            c,
                            ContentBlock::Text { text } if text.contains(MARK_OK)
                        ))
                    )),
                    "salvaged message must be persisted in the session transcript"
                );
                assert!(
                    !s.ctx.has_messages(&s.alice),
                    "no social-inbox requeue: message must not be delivered twice"
                );

                // ACK boundary: batch row r1 was acked by the runner's ordinary
                // success path; the monitor-drained row r2 — consumed and
                // model-visible — is STILL pending.
                assert_eq!(
                    s.journal.pending_count().unwrap(),
                    1,
                    "monitor-drained message must remain durably pending after delivery"
                );
                let remaining = s.journal.recover().unwrap();
                assert_eq!(remaining.len(), 1);
                assert_eq!(
                    remaining[0].id, r2,
                    "the pending row must be the monitor-drained message"
                );
                assert_eq!(
                    remaining[0].message.body, MARK_OK,
                    "original body/identity preserved in the durable row"
                );

                // Recovery: a fresh InboxStore over the same journal redelivers
                // the still-pending message (crash/restart path).
                let restored = crate::inbox::InboxStore::with_journal(s.journal.clone()).unwrap();
                let recovered = restored.drain_all(&s.alice);
                assert_eq!(recovered.len(), 1, "restart must redeliver row r2");
                assert_eq!(recovered[0].journal_id, Some(r2));
                assert_eq!(recovered[0].body, MARK_OK);
            })
            .catch_unwind()
            .await;
            s.release.notify_one();
            s.ctx.abort_monitors();
            s.runner.abort();
            let _ = s.runner.await;
            if let Err(panic) = outcome {
                std::panic::resume_unwind(panic);
            }
        }

        // ── Scenario 2: salvage; continuation run's provider FAILS ──────────
        const MARK_FAIL: &str = "MONITOR-SALVAGE-FAIL-b9c2";
        {
            let mut s = spawn_scenario(true).await;
            let outcome = std::panic::AssertUnwindSafe(async {

            push(&s, "batch work for alice");
            let r1 = s.journal.recover().unwrap().last().unwrap().id;
            wait_until(
                || s.client.call_count.load(Ordering::SeqCst) == 1,
                "run 1 to issue its provider request",
            ).await;
            wait_until(
                || s.parked.load(Ordering::SeqCst),
                "run 1 to park at its final stop decision",
            ).await;

            push(&s, MARK_FAIL);
            let r2 = s.journal.recover().unwrap().last().unwrap().id;
            assert_ne!(r1, r2, "distinct durable rows");
            wait_until(
                || !s.ctx.has_messages(&s.alice),
                "production monitor to drain the mid-run message",
            ).await;
            wait_until(
                || s.harness.has_queued_messages(),
                "production monitor to complete steering into the live channel",
            ).await;
            assert_eq!(s.ctx.get_agent_info(&s.alice).unwrap().status, AgentStatus::Busy);
            assert_eq!(s.journal.pending_count().unwrap(), 2);

            s.release.notify_one();
            // With the runner fix, the failed continuation requeues the batch;
            // the loop retries (mock responds "salvage consumed"), which ACKs
            // r1. Settle on the durable end state: r1 gone (acked exactly at
            // the successful-redelivery boundary), r2 still pending for
            // restart recovery.
            wait_until(
                || s.journal.pending_count().unwrap() == 1,
                "runner to retry the requeued batch and ack r1",
            ).await;
            wait_until(
                || {
                    s.ctx.get_agent_info(&s.alice).map(|i| i.status)
                        == Some(AgentStatus::Idle)
                },
                "runner to settle after the retry",
            ).await;

                let continuation = s.continuation.snapshot();
                assert_eq!(continuation.salvage_flag, Some(true));
                assert_eq!(continuation.queued_at_disposition, Some(true));
            assert!(continuation.entered);
                assert_eq!(continuation.result.as_deref(), Some("Ok(())"));
                assert_eq!(
                    continuation.internal_error_at_completion.as_deref(),
                    Some("llm provider error: rate limited (retry after None)")
                );
                // Run 1 + failed continuation + successful retry.
                assert_eq!(s.client.call_count.load(Ordering::SeqCst), 3);
                assert_eq!(s.harness.state().phase, llm_harness_types::HarnessPhase::Idle);
            let mut orphaned_r2 = false;
            while let Ok(event) = s.events.try_recv() {
                if let llm_harness_agent::AgentHarnessEvent::Agent(
                    llm_harness_types::AgentEvent::SteerOrphaned { messages },
                ) = event.as_ref()
                {
                    orphaned_r2 |= messages.iter().any(|message| matches!(message,
                        AgentMessage::User(user) if user.content.iter().any(|part| matches!(part,
                            ContentBlock::Text { text } if text.contains(MARK_FAIL)))));
                }
            }
            assert!(orphaned_r2, "run 1 must hand r2 into harness salvage");
            assert!(!s.harness.has_queued_messages(), "retry consumed the harness buffer");

            // A rejected provider request still contains the freshly delivered
            // marker. Count its first introduction, separately from replay.
            let deliveries = new_delivery_count(&s.client.captured_requests.lock(), MARK_FAIL);
            assert_eq!(
                deliveries, 1,
                "r2 must first enter a provider request in the continuation"
            );
            let remaining = s.journal.recover().unwrap();

            // Redelivery proof: the runner issued a THIRD provider call only
            // because the failed continuation requeued the batch — a silent
            // ack would have ended the loop at two calls.

            // No live queue holds the failed message; only the journal can
            // recover it — verified by the fresh-store redelivery above.
            assert!(
                !s.ctx.has_messages(&s.alice),
                "failed delivery left nothing in the volatile inbox"
            );
            let restored = crate::inbox::InboxStore::with_journal(s.journal.clone()).unwrap();
            let recovered = restored.drain_all(&s.alice);
            assert!(recovered.iter().any(|m| m.body == MARK_FAIL));
            assert_eq!(recovered[0].journal_id, Some(r2));

            // Corrected ack boundary: r1 is acked ONLY by the successful
            // retry (the batch was redelivered to the model); the salvage
            // row r2 remains pending for crash/restart recovery.
            assert_eq!(
                remaining.iter().map(|row| row.id).collect::<Vec<_>>(),
                vec![r2],
                "r1 acked exactly at the successful-redelivery boundary; r2 stays pending for restart recovery: continuation={continuation:?}, provider_calls={}",
                s.client.call_count.load(Ordering::SeqCst),
            );

            }).catch_unwind().await;
            s.release.notify_one();
            s.ctx.abort_monitors();
            s.runner.abort();
            let _ = s.runner.await;
            if let Err(panic) = outcome {
                std::panic::resume_unwind(panic);
            }
        }
    }
}
