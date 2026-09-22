//! Per-agent inbox: priority queue of messages with FIFO tie-breaking.

use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::time::{Duration, sleep};

use crate::AgentId;

/// Classification of an inbox message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageType {
    /// Direct peer-to-peer message.
    Direct,
    /// A request asking this agent to take on a delegated task.
    DelegationRequest,
    /// A team-wide broadcast.
    Broadcast,
    /// A timeout notification from the TimeoutManager.
    Timeout,
    /// A self-reminder set by the agent via the set_reminder tool.
    SelfReminder,
    /// A self-triggered reflection task: the agent has accumulated enough
    /// run history and should review past transcripts to extract lessons.
    ReflectionTrigger,
}

/// Server-defined conversation view for a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatView {
    /// Original message in the shared team group chat.
    GroupMessage,
    /// Member reply that belongs in the shared team group chat.
    GroupReply,
    /// Operator-to-member private conversation message.
    DirectMessage,
}

impl ChatView {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GroupMessage => "group_message",
            Self::GroupReply => "group_reply",
            Self::DirectMessage => "direct_message",
        }
    }
}

/// Priority levels for inbox messages. Higher = processed first.
pub mod priority {
    /// Operator-submitted task (highest — human-initiated).
    pub const OPERATOR: u32 = 100;
    /// Timeout notification — urgent, the agent must act on it.
    pub const TIMEOUT: u32 = 50;
    /// Self-reminder — important but not urgent.
    pub const SELF_REMINDER: u32 = 30;
    /// Delegation request from a peer — slightly above normal messages.
    pub const DELEGATION: u32 = 1;
    /// Self-triggered reflection — ahead of ordinary traffic but behind
    /// human/operator work and reminders (legacy value from the original
    /// agent-society design; changing it would reorder delivery).
    pub const REFLECTION: u32 = 10;
    /// Normal peer-to-peer or broadcast message.
    pub const NORMAL: u32 = 0;
}

/// A message delivered to an agent's inbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    /// Sender of the message.
    pub from: AgentId,
    /// Recipient the message was addressed to.
    pub to: AgentId,
    /// Message body text.
    pub body: String,
    /// Delivery semantics of the message (normal, timeout, reflection…).
    pub message_type: MessageType,
    /// Queue priority; see the `priority` module constants for tiers.
    pub priority: u32,
    /// Stable logical identity shared by live delivery and chat history.
    #[serde(default)]
    pub message_id: Option<String>,
    /// Correlation id for delegation/broadcast timeouts, when applicable.
    pub task_id: Option<String>,
    /// Correlation id shared by all messages of one broadcast, when applicable.
    pub broadcast_id: Option<String>,
    /// Durable journal row id; `None` for tests, legacy snapshots, and teams
    /// assembled without a journal.
    #[serde(skip)]
    pub journal_id: Option<i64>,
}

/// Internal heap entry: pairs a message with its priority and insertion
/// sequence number so that higher priority pops first and ties break FIFO.
#[derive(Debug)]
struct PriorityEntry {
    priority: u32,
    seq: u64,
    message: InboxMessage,
}

impl PartialEq for PriorityEntry {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}

impl Eq for PriorityEntry {}

impl PartialOrd for PriorityEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PriorityEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap: we want highest priority first.
        // For equal priority, lower seq (earlier insertion) should pop first,
        // so we invert the seq comparison.
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

/// A priority queue for a single agent's inbox.
pub struct InboxQueue {
    heap: BinaryHeap<PriorityEntry>,
    next_seq: u64,
    /// Woken whenever this queue receives a message. Shared via `Arc` so
    /// waiters can hold a registration across the lock release.
    notify: Arc<tokio::sync::Notify>,
}

impl Default for InboxQueue {
    fn default() -> Self {
        Self {
            heap: BinaryHeap::new(),
            next_seq: 0,
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

impl InboxQueue {
    /// Create an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a message onto the queue. Its `priority` field determines ordering.
    pub fn push(&mut self, message: InboxMessage) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let priority = message.priority;
        self.heap.push(PriorityEntry {
            priority,
            seq,
            message,
        });
        // Wake only this queue's waiters — one notification per push.
        self.notify.notify_one();
    }

    /// The queue's wakeup handle, for waiters to register against.
    fn notify_handle(&self) -> Arc<tokio::sync::Notify> {
        self.notify.clone()
    }

    /// Pop the highest-priority message (FIFO within equal priority).
    pub fn pop(&mut self) -> Option<InboxMessage> {
        self.heap.pop().map(|e| e.message)
    }

    /// Whether the queue holds no messages.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Number of messages in the queue.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Drain all messages, returning them in priority order.
    pub fn drain_sorted(&mut self) -> Vec<InboxMessage> {
        let mut out = Vec::with_capacity(self.heap.len());
        while let Some(entry) = self.heap.pop() {
            out.push(entry.message);
        }
        out
    }
}

/// Per-agent inbox store, safe for concurrent access.
pub struct InboxStore {
    queues: Mutex<HashMap<AgentId, InboxQueue>>,
    journal: Option<Arc<crate::inbox_journal::InboxJournal>>,
}

impl Default for InboxStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InboxStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            queues: Mutex::new(HashMap::new()),
            journal: None,
        }
    }

    /// Create a store backed by a durable pending-message journal. Messages
    /// queued through `push` are journaled first; returned/restored messages
    /// retain their existing journal row.
    pub fn with_journal(
        journal: Arc<crate::inbox_journal::InboxJournal>,
    ) -> Result<Self, crate::inbox_journal::InboxJournalError> {
        let store = Self {
            queues: Mutex::new(HashMap::new()),
            journal: Some(journal.clone()),
        };
        for pending in journal.recover()? {
            let mut message = pending.message;
            message.journal_id = Some(pending.id);
            if message.message_id.is_none() {
                message.message_id = Some(match (&message.message_type, &message.broadcast_id) {
                    (MessageType::Broadcast, Some(broadcast_id)) => {
                        format!("legacy-broadcast-{broadcast_id}")
                    }
                    _ => uuid::Uuid::now_v7().to_string(),
                });
            }
            store.push_returned(message);
        }
        Ok(store)
    }

    /// Push a durable message into the recipient's queue and wake exactly the
    /// recipient's waiters — pushes for other agents never steal the wakeup.
    ///
    /// Returns an error without enqueuing when the journal write fails, so
    /// callers cannot mistake an unlogged message for durable delivery.
    pub fn push(
        &self,
        message: InboxMessage,
    ) -> Result<(), crate::inbox_journal::InboxJournalError> {
        self.push_for_observer(message).map(|_| ())
    }

    /// Push a durable message and return the stored form, including its
    /// journal row id. Observers need that id to acknowledge delivery.
    pub fn push_for_observer(
        &self,
        message: InboxMessage,
    ) -> Result<InboxMessage, crate::inbox_journal::InboxJournalError> {
        let mut message = message;
        if let Some(journal) = &self.journal
            && message.journal_id.is_none()
        {
            message.journal_id = Some(journal.enqueue(&message)?);
        }
        let to = message.to.clone();
        let mut queues = self.queues.lock();
        queues.entry(to).or_default().push(message.clone());
        Ok(message)
    }

    /// Requeue an already-journaled message without creating a new row.
    pub fn push_returned(&self, message: InboxMessage) {
        let to = message.to.clone();
        let mut queues = self.queues.lock();
        queues.entry(to).or_default().push(message);
    }

    /// Drain all messages for `agent_id`, returning them in priority order.
    pub fn drain_all(&self, agent_id: &AgentId) -> Vec<InboxMessage> {
        let mut queues = self.queues.lock();
        match queues.get_mut(agent_id) {
            Some(q) => q.drain_sorted(),
            None => Vec::new(),
        }
    }

    /// Drain every queue, with each recipient's messages in priority order.
    pub fn drain_everything(&self) -> Vec<InboxMessage> {
        let mut queues = self.queues.lock();
        let mut messages = Vec::new();
        for (_, mut queue) in queues.drain() {
            messages.extend(queue.drain_sorted());
        }
        messages
    }

    /// Restore previously drained messages. Message priority controls the
    /// rebuilt queue order; contacts and registered recipients remain the
    /// caller's responsibility.
    pub fn restore(&self, messages: Vec<InboxMessage>) {
        for message in messages {
            self.push_returned(message);
        }
    }

    /// Acknowledge messages whose runner completed successfully. Ack failures
    /// are non-fatal: the next crash recovery will redeliver the message.
    pub fn ack(&self, messages: &[InboxMessage]) {
        let Some(journal) = &self.journal else {
            return;
        };
        for message in messages.iter().filter_map(|message| message.journal_id) {
            if let Err(error) = journal.ack(message) {
                tracing::error!(journal_id = message, error = %error, "failed to ack inbox message");
            }
        }
    }

    /// Whether `agent_id` has any pending messages.
    pub fn has_messages(&self, agent_id: &AgentId) -> bool {
        let queues = self.queues.lock();
        queues.get(agent_id).is_some_and(|q| !q.is_empty())
    }

    /// Asynchronously wait until at least one message is available for
    /// `agent_id`.
    ///
    /// Wakes come from a per-agent Notify (stored alongside the queue). The
    /// 250 ms polling fallback remains as a safety net for the
    /// check-then-await race window.
    pub async fn wait_for_message(&self, agent_id: &AgentId) {
        // Register on this agent's Notify. Taking the handle under the
        // lock (entry().or_default()) ensures the queue — and its
        // stable per-agent Notify — exists before we await; the Arc
        // keeps the Notify alive across the lock release. The handle is
        // invariant per agent, so it is acquired once, outside the loop.
        let handle = {
            let mut queues = self.queues.lock();
            queues.entry(agent_id.clone()).or_default().notify_handle()
        };
        loop {
            if self.has_messages(agent_id) {
                return;
            }
            let notified = handle.notified();
            tokio::select! {
                _ = notified => {}
                _ = sleep(Duration::from_millis(250)) => {}
            }
        }
    }
}

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;

    fn msg(from: &str, to: &str, body: &str, priority: u32) -> InboxMessage {
        InboxMessage {
            journal_id: None,
            from: AgentId::new(from),
            to: AgentId::new(to),
            body: body.to_string(),
            message_type: MessageType::Direct,
            priority,
            task_id: None,
            broadcast_id: None,
            message_id: None,
        }
    }

    /// Push a message into the queue WITHOUT firing the Notify wakeup —
    /// simulates a notification lost before the waiter registered, the
    /// exact case the polling fallback exists to cover.
    fn push_silent(store: &InboxStore, message: InboxMessage) {
        let mut queues = store.queues.lock();
        let queue = queues.entry(message.to.clone()).or_default();
        let seq = queue.next_seq;
        queue.next_seq += 1;
        let priority = message.priority;
        queue.heap.push(PriorityEntry {
            priority,
            seq,
            message,
        });
    }

    #[test]
    fn queue_pops_highest_priority_first() {
        let mut q = InboxQueue::new();
        q.push(msg("a", "b", "low", 1));
        q.push(msg("a", "b", "high", 10));
        q.push(msg("a", "b", "mid", 5));

        assert_eq!(q.pop().unwrap().body, "high");
        assert_eq!(q.pop().unwrap().body, "mid");
        assert_eq!(q.pop().unwrap().body, "low");
    }

    #[test]
    fn queue_fifo_same_priority() {
        let mut q = InboxQueue::new();
        q.push(msg("a", "b", "first", 5));
        q.push(msg("a", "b", "second", 5));
        q.push(msg("a", "b", "third", 5));

        assert_eq!(q.pop().unwrap().body, "first");
        assert_eq!(q.pop().unwrap().body, "second");
        assert_eq!(q.pop().unwrap().body, "third");
    }

    #[test]
    fn queue_empty_returns_none() {
        let mut q = InboxQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        assert!(q.pop().is_none());
    }

    #[test]
    fn inbox_store_push_and_drain() {
        let store = InboxStore::new();
        let alice = AgentId::new("alice");

        store.push(msg("bob", "alice", "hello", 1)).unwrap();
        store.push(msg("carol", "alice", "urgent", 10)).unwrap();
        store.push(msg("dave", "alice", "normal", 5)).unwrap();

        assert!(store.has_messages(&alice));

        let drained = store.drain_all(&alice);
        assert_eq!(drained.len(), 3);
        // Highest priority first.
        assert_eq!(drained[0].body, "urgent");
        assert_eq!(drained[1].body, "normal");
        assert_eq!(drained[2].body, "hello");

        // After draining, no messages remain.
        assert!(!store.has_messages(&alice));
        assert!(store.drain_all(&alice).is_empty());
    }

    #[test]
    fn drain_everything_and_restore_round_trips_priority() {
        let store = InboxStore::new();
        let alice = AgentId::new("alice");
        store
            .push(msg("bob", "alice", "normal", priority::NORMAL))
            .unwrap();
        store
            .push(msg("operator", "alice", "operator", priority::OPERATOR))
            .unwrap();

        let messages = store.drain_everything();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].body, "operator");
        assert_eq!(messages[1].body, "normal");
        assert!(!store.has_messages(&alice));

        let restored = InboxStore::new();
        restored.restore(messages);
        let messages = restored.drain_all(&alice);
        assert_eq!(messages[0].body, "operator");
        assert_eq!(messages[1].body, "normal");
    }

    #[test]
    fn journaled_message_survives_store_drop_until_acked() {
        let temp = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            crate::inbox_journal::InboxJournal::open(temp.path().join("inbox.sqlite3")).unwrap(),
        );
        let alice = AgentId::new("alice");

        let store = InboxStore::with_journal(Arc::clone(&journal)).unwrap();
        store
            .push(msg("operator", "alice", "crash safe", priority::OPERATOR))
            .unwrap();
        let drained = store.drain_all(&alice);
        assert_eq!(drained.len(), 1);
        assert_eq!(journal.recover().unwrap().len(), 1);

        drop(store);
        let store = InboxStore::with_journal(Arc::clone(&journal)).unwrap();
        assert!(store.has_messages(&alice));

        store.ack(&drained);
        assert!(journal.recover().unwrap().is_empty());
    }

    #[test]
    fn journal_failure_does_not_enqueue_message() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("inbox.sqlite3");
        let journal = Arc::new(crate::inbox_journal::InboxJournal::open(&path).unwrap());
        let store = InboxStore::with_journal(Arc::clone(&journal)).unwrap();
        let alice = AgentId::new("alice");

        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute("DROP TABLE pending_messages", [])
            .unwrap();

        let error = store
            .push(msg("bob", "alice", "lost", priority::NORMAL))
            .unwrap_err();
        assert!(matches!(
            error,
            crate::inbox_journal::InboxJournalError::Sqlite(_)
        ));
        assert!(!store.has_messages(&alice));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_message_wakes_only_on_own_messages() {
        // Per-agent Notify: a push for bob must NOT wake alice's waiter.
        // Under paused time the 10 ms polling fallback never advances, so
        // the only way alice's waiter can return is a direct wakeup.
        let store = std::sync::Arc::new(InboxStore::new());
        let alice = AgentId::new("alice");

        let alice_waiter = {
            let store = store.clone();
            tokio::spawn(async move { store.wait_for_message(&alice).await })
        };

        // Let alice's waiter reach the select.
        tokio::time::sleep(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;

        // Push to bob — must not wake alice.
        store.push(msg("carol", "bob", "for bob", 1));
        tokio::time::sleep(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(
            !alice_waiter.is_finished(),
            "push for another agent must not wake alice's waiter"
        );

        // Push to alice — must wake her promptly.
        store.push(msg("carol", "alice", "for alice", 1));
        tokio::time::sleep(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(alice_waiter.is_finished(), "own push must wake the waiter");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_wait_wakes_on_notify_within_50ms() {
        // Regression guard for the idle-poll fallback: an idle waiter must
        // NOT return purely from polling while time advances through the
        // first 50 ms — it must still be parked. (With the old 10 ms poll
        // this test failed: a silently dropped message was discovered by
        // polling within the window.) Delivery then completes via notify.
        let store = std::sync::Arc::new(InboxStore::new());
        let alice = AgentId::new("alice");

        let alice_waiter = {
            let store = store.clone();
            tokio::spawn(async move { store.wait_for_message(&alice).await })
        };

        // Let alice's waiter reach the select before any timing starts.
        tokio::time::sleep(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;

        // Deliver a message without firing Notify (lost-wakeup case).
        push_silent(&store, msg("bob", "alice", "lost wakeup", 1));
        tokio::task::yield_now().await;

        // Advance time in 10 ms steps through the first 50 ms; the waiter
        // must stay parked the whole way (250 ms fallback ⇒ no poll fires).
        for _ in 0..5 {
            tokio::time::advance(Duration::from_millis(10)).await;
            assert!(
                !alice_waiter.is_finished(),
                "waiter must not complete via polling within 50 ms"
            );
        }

        // A push now fires Notify and must wake the waiter promptly.
        store.push(msg("carol", "alice", "for alice", 1));
        tokio::time::sleep(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(
            alice_waiter.is_finished(),
            "notify must wake the waiter after the 50 ms window"
        );
    }
}
