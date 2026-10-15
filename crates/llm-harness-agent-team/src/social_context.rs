//! Social context: shared state holding the agent registry, inboxes,
//! contact store, team directory, and relation engine.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::task::AbortHandle;

use crate::agent_id::AgentId;
use crate::chat_history_journal::ChatHistoryEntry;
use crate::contact::{ContactRecord, ContactStore};
use crate::inbox::{InboxMessage, InboxStore, MessageType, TaskMessageKind};
use crate::relation::RelationEngine;
use crate::timeout_manager::reserved;

/// Outcome of cancelling a pending timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeoutCancel {
    /// A pending timeout was found and aborted. `peer` is the remote agent
    /// the request targeted, if it had one (delegation timeouts carry a
    /// peer; broadcast and self-reminder timeouts do not).
    Cancelled {
        /// Remote agent of the cancelled request, when one was recorded.
        peer: Option<AgentId>,
    },
    /// Nothing was pending under `(agent, key)`.
    NotFound,
}

/// One deadline cancelled by
/// [`SocialContext::cancel_timeouts_for_messages`]: the correlation key
/// (`task_id` or `broadcast_id` of the awaited request) and the remote
/// peer the request targeted, if any (delegation only).
///
/// Returning the identity — instead of only the aborted
/// [`AbortHandle`] — lets a caller re-arm the same deadline later via
/// [`SocialContext::re_register_timeout`] (e.g. the agent re-broadcasts and the
/// flow wants to restore its cancelled follow-up deadline under the same
/// correlation key). The aborted handle itself is dead and unusable for
/// re-registration, so it is deliberately not returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelledTimeout {
    /// Correlation key of the cancelled deadline.
    pub key: String,
    /// Remote agent the cancelled request targeted, if recorded.
    pub peer: Option<AgentId>,
}

/// Logical and correlation IDs for one group broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastMessageIds {
    /// Stable identity of the logical group message.
    pub message_id: String,
    /// Shared correlation key carried by all recipient deliveries and replies.
    pub broadcast_id: String,
}

fn generate_message_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// One pending entry: `(AbortHandle, Option<AgentId>)` — the remote peer
/// the request targeted (delegation only).
type PendingEntry = (AbortHandle, Option<AgentId>);

/// Tracks pending and fired timeouts per agent.
///
/// Each pending entry maps `(agent, key) -> (AbortHandle, Option<peer>)`.
/// The key is the `task_id` or `broadcast_id` of the originating request;
/// the peer is the remote agent the request was addressed to (delegation
/// replies) — used to emit relation events on cancel/fire. Entries without
/// a peer (broadcasts, self-reminders) never emit relation events.
///
/// Lifecycle:
/// - `register` adds to `pending`.
/// - When the timeout fires naturally, `mark_fired` moves the key from
///   `pending` to `fired`. Late replies keyed by a fired timeout are
///   silently dropped so the agent isn't bothered with stale responses.
///   The fired set is bounded: at most [`MAX_FIRED_KEYS_PER_AGENT`] keys
///   per agent, oldest evicted first.
/// - When a reply arrives in time, `cancel` aborts the task and removes
///   the key from both `pending` and `fired`.
#[derive(Default)]
pub struct TimeoutRegistry {
    pending: Mutex<HashMap<AgentId, HashMap<String, PendingEntry>>>,
    /// Keys whose timeout already fired, per agent (FIFO order). Late
    /// replies carrying a fired key are dropped by `drain_inbox`. Bounded:
    /// at most [`MAX_FIRED_KEYS_PER_AGENT`] keys per agent, oldest evicted
    /// first (keys are monotonic counters or UUIDs and never reused, so an
    /// evicted key's very-late reply may be delivered instead of dropped —
    /// an acceptable cost vs. unbounded growth on long-lived teams).
    fired: Mutex<HashMap<AgentId, VecDeque<String>>>,
}

/// Upper bound on remembered fired keys per agent (late-reply filtering).
const MAX_FIRED_KEYS_PER_AGENT: usize = 1024;

impl TimeoutRegistry {
    /// Register a pending timeout handle under `(agent, key)`.
    ///
    /// `peer` is the remote agent the request targets (`Some` for
    /// delegation timeouts, `None` for broadcasts and self-reminders) —
    /// it is handed back by `cancel`/`mark_fired` for relation events.
    ///
    /// Returns `true` if a pending timeout was replaced under this key
    /// (its timer task has been aborted), `false` if the key was fresh.
    ///
    /// Note: a timeout with `after == 0` can fire before its registration
    /// runs, parking a dead handle in `pending` (a one-off leak, self-
    /// limiting). All tool entry points validate `after > 0`, so this is
    /// only reachable via direct library use.
    pub fn register(
        &self,
        agent: &AgentId,
        key: String,
        peer: Option<AgentId>,
        handle: AbortHandle,
    ) -> bool {
        let old = self
            .pending
            .lock()
            .entry(agent.clone())
            .or_default()
            .insert(key, (handle, peer));
        let replaced = old.is_some();
        // Overwriting a key must abort the previous timer task, or the
        // orphaned handle keeps its tokio::time::sleep alive and fires
        // a duplicate timeout/reminder (e.g. the LLM reuses a task_id
        // across two set_reminder calls).
        if let Some((old_handle, _)) = old {
            old_handle.abort();
        }
        replaced
    }

    /// Remove a pending timeout entry without aborting (used by
    /// self-reminder cleanup after firing — no late-reply filtering needed).
    pub fn remove(&self, agent: &AgentId, key: &str) {
        let mut guard = self.pending.lock();
        if let Some(bucket) = guard.get_mut(agent) {
            bucket.remove(key);
            if bucket.is_empty() {
                guard.remove(agent);
            }
        }
    }

    /// Mark a timeout as fired (called by the spawned task after it has
    /// pushed the timeout message). Moves the key from `pending` to
    /// `fired` so that late replies carrying this key are dropped.
    ///
    /// Returns the peer recorded at registration, if any, so the caller
    /// can emit a relation event for the failed collaboration.
    pub fn mark_fired(&self, agent: &AgentId, key: &str) -> Option<AgentId> {
        let peer = {
            let mut pending = self.pending.lock();
            let peer = pending
                .get_mut(agent)
                .and_then(|bucket| bucket.remove(key))
                .and_then(|(_, peer)| peer);
            if let Some(bucket) = pending.get_mut(agent)
                && bucket.is_empty()
            {
                pending.remove(agent);
            }
            peer
        };
        let mut fired = self.fired.lock();
        let bucket = fired.entry(agent.clone()).or_default();
        bucket.push_back(key.to_string());
        while bucket.len() > MAX_FIRED_KEYS_PER_AGENT {
            bucket.pop_front();
        }
        peer
    }

    /// Abort and remove a pending timeout. The result distinguishes the
    /// three outcomes callers care about:
    /// - `Cancelled { peer: Some(_) }`: a delegation timeout was cancelled
    ///   (its reply arrived in time) — emit a `SuccessfulCollaboration`
    ///   relation event from the canceller toward that peer.
    /// - `Cancelled { peer: None }`: a peer-less timeout (broadcast,
    ///   self-reminder) was cancelled — emit nothing.
    /// - `NotFound`: nothing was pending under `(agent, key)`.
    pub fn cancel(&self, agent: &AgentId, key: &str) -> TimeoutCancel {
        let mut pending = self.pending.lock();
        let removed = pending.get_mut(agent).and_then(|bucket| bucket.remove(key));
        if let Some(bucket) = pending.get_mut(agent)
            && bucket.is_empty()
        {
            pending.remove(agent);
        }
        drop(pending);
        // Also remove from fired set — this is a no-op if the timeout
        // was cancelled before firing, and cleans up the fired entry
        // if it already fired (reply arrived after firing but before
        // the agent processed the timeout message).
        let mut fired = self.fired.lock();
        if let Some(bucket) = fired.get_mut(agent) {
            if let Some(pos) = bucket.iter().position(|k| k == key) {
                bucket.remove(pos);
            }
            if bucket.is_empty() {
                fired.remove(agent);
            }
        }
        match removed {
            Some((handle, peer)) => {
                handle.abort();
                TimeoutCancel::Cancelled { peer }
            }
            None => TimeoutCancel::NotFound,
        }
    }

    /// Returns `true` if the timeout for `(agent, key)` has fired —
    /// late replies carrying this key should be dropped.
    pub fn has_fired(&self, agent: &AgentId, key: &str) -> bool {
        self.fired
            .lock()
            .get(agent)
            .is_some_and(|bucket| bucket.iter().any(|k| k == key))
    }

    /// Number of timer tasks currently tracked for shutdown/cancellation.
    pub fn pending_count(&self) -> usize {
        self.pending
            .lock()
            .values()
            .map(|bucket| bucket.len())
            .sum()
    }

    /// Returns the peer recorded at registration for `(agent, key)`, if a
    /// pending entry exists.
    ///
    /// The outer `Option` distinguishes "no such entry" (`None`) from
    /// "entry exists" (`Some`); the inner `Option` is the registered peer
    /// (`None` for broadcasts and self-reminders, `Some(peer)` for
    /// delegations). [`SocialContext::cancel_timeouts_for_messages`] uses
    /// this to enforce that a peer-carrying registration may only be
    /// cancelled by a reply from that peer.
    pub fn peer_of(&self, agent: &AgentId, key: &str) -> Option<Option<AgentId>> {
        self.pending
            .lock()
            .get(agent)
            .and_then(|bucket| bucket.get(key))
            .map(|(_, peer)| peer.clone())
    }

    /// Remove the fired mark for `(agent, key)` (no-op if not fired).
    ///
    /// Called when the agent actually consumes the Timeout notification
    /// (`SocialContext::drain_inbox`): the mark exists only to filter late
    /// replies until the agent has seen the timeout, so it must not
    /// outlive the notification. Without this, draining leaves the key
    /// marked fired and later replies for the same key are mis-handled as
    /// "already fired".
    pub fn clear_fired(&self, agent: &AgentId, key: &str) {
        let mut fired = self.fired.lock();
        if let Some(bucket) = fired.get_mut(agent) {
            if let Some(pos) = bucket.iter().position(|k| k == key) {
                bucket.remove(pos);
            }
            if bucket.is_empty() {
                fired.remove(agent);
            }
        }
    }

    /// Abort every pending timeout and clear the registry.
    ///
    /// Called at team shutdown: timers left running would later fire into
    /// dead inboxes and could emit spurious relation events against
    /// stopped members. The `fired` set is cleared too — no reply can
    /// arrive after shutdown, so late-reply filtering is moot.
    pub fn cancel_all(&self) -> Vec<(AgentId, String)> {
        let mut pending = self.pending.lock();
        let cancelled = pending
            .iter()
            .flat_map(|(agent, bucket)| bucket.keys().map(move |key| (agent.clone(), key.clone())))
            .collect();
        for bucket in pending.values_mut() {
            for (handle, _) in bucket.values() {
                handle.abort();
            }
        }
        pending.clear();
        self.fired.lock().clear();
        cancelled
    }
    /// Restore a previously cancelled (or fired) deadline under
    /// `(agent, key)` with a live timer's abort handle.
    ///
    /// This is the re-arm half of
    /// [`SocialContext::cancel_timeouts_for_messages`]: that method returns
    /// the cancelled `CancelledTimeout { key, peer }` identities so a flow
    /// that later re-registers (e.g. re-broadcasts and wants its follow-up
    /// deadline back) can restore the entry under the same correlation key
    /// without blind re-registration.
    ///
    /// Semantics mirror [`Self::register`]: any entry already pending
    /// under the key is aborted and replaced, and any stale fired mark
    /// for the key is cleared (a re-armed deadline must be able to fire
    /// and filter late replies again). `peer` is the remote agent the
    /// request targets (`Some` for delegation timeouts) — it is handed
    /// back by `cancel`/`mark_fired` for relation events.
    ///
    /// Returns `true` if a pending timeout was replaced under this key
    /// (its timer task has been aborted), `false` if the key was fresh.
    ///
    /// Re-registering over an already-pending entry aborts the just-inserted
    /// handle: only a live `AbortHandle` from a fresh timer can take over
    /// the slot (fresh-handle contract; mirrors [`Self::register`]).
    pub fn re_register(
        &self,
        agent: &AgentId,
        key: String,
        peer: Option<AgentId>,
        handle: AbortHandle,
    ) -> bool {
        let old = self
            .pending
            .lock()
            .entry(agent.clone())
            .or_default()
            .insert(key.clone(), (handle, peer));
        let replaced = old.is_some();
        if let Some((old_handle, _)) = old {
            old_handle.abort();
        }
        // A restored deadline must be able to fire (and filter late
        // replies) again — drop any stale fired mark from the previous
        // lifecycle of this key.
        let mut fired = self.fired.lock();
        if let Some(bucket) = fired.get_mut(agent) {
            if let Some(pos) = bucket.iter().position(|k| *k == key) {
                bucket.remove(pos);
            }
            if bucket.is_empty() {
                fired.remove(agent);
            }
        }
        replaced
    }
}

/// Lifecycle status of a registered agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// Registered and waiting for work.
    Idle,
    /// Currently processing a run.
    Busy,
    /// Shut down; no longer accepts messages.
    Stopped,
}

/// Metadata for a registered agent.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    /// Id of the agent.
    pub id: AgentId,
    /// One-line description of the agent's specialty.
    pub expertise: String,
    /// Current lifecycle status.
    pub status: AgentStatus,
}

/// A registry mapping agent IDs to their metadata.
///
/// Serves both point lookups (by id) and full-team iteration (for broadcast).
pub struct AgentRegistry {
    agents: Mutex<HashMap<AgentId, AgentInfo>>,
}

impl AgentRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            agents: Mutex::new(HashMap::new()),
        }
    }

    /// Insert or replace an agent's metadata.
    pub fn register(&self, info: AgentInfo) {
        let mut agents = self.agents.lock();
        agents.insert(info.id.clone(), info);
    }

    /// Look up an agent's metadata by id.
    pub fn get(&self, id: &AgentId) -> Option<AgentInfo> {
        self.agents.lock().get(id).cloned()
    }

    /// Set an agent's status; returns `false` when the id is unknown.
    pub fn update_status(&self, id: &AgentId, status: AgentStatus) -> bool {
        let mut agents = self.agents.lock();
        if let Some(info) = agents.get_mut(id) {
            info.status = status;
            return true;
        }
        false
    }

    /// Remove an agent from the registry.
    pub fn remove(&self, id: &AgentId) {
        self.agents.lock().remove(id);
    }

    /// Return all registered agents.
    pub fn list(&self) -> Vec<AgentInfo> {
        self.agents.lock().values().cloned().collect()
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared social state accessible to all agents and tools.
///
/// Holds `Arc` references to the registry, inbox store, contact store,
/// and relation engine. Cloning is cheap (Arc bumps).
#[derive(Clone)]
pub struct SocialContext {
    registry: Arc<AgentRegistry>,
    inboxes: Arc<InboxStore>,
    chat_history: Arc<Mutex<VecDeque<InboxMessage>>>,
    contacts: Arc<ContactStore>,
    relations: Arc<RelationEngine>,
    /// Timeout bookkeeping shared with [`crate::timeout_manager::TimeoutManager`].
    timeouts: Arc<TimeoutRegistry>,
    timer_journal: Option<Arc<crate::timer_journal::TimeoutJournal>>,
    /// Durable transcript for all social messages. Unlike the inbox journal,
    /// rows are never deleted when a message is consumed.
    chat_history_journal: Option<Arc<crate::chat_history_journal::ChatHistoryJournal>>,
    /// Live per-agent monitor tasks, keyed by agent id. Registered by
    /// [`crate::agent_runner::spawn_monitor_task`] so shutdown can abort
    /// monitors whose runner died without running its own cleanup.
    monitor_registry: Arc<Mutex<HashMap<AgentId, AbortHandle>>>,
    controlled_tasks: Arc<crate::controlled::ControlledTaskRegistry>,
    max_contacts: usize,
}

impl SocialContext {
    /// Create a new social context with the given capacity limits.
    pub fn new(max_contacts: usize) -> Self {
        Self {
            registry: Arc::new(AgentRegistry::new()),
            inboxes: Arc::new(InboxStore::new()),
            contacts: Arc::new(ContactStore::new()),
            relations: Arc::new(RelationEngine::default()),
            timeouts: Arc::new(TimeoutRegistry::default()),
            timer_journal: None,
            chat_history_journal: None,
            monitor_registry: Arc::new(Mutex::new(HashMap::new())),
            controlled_tasks: Arc::new(crate::controlled::ControlledTaskRegistry::default()),
            chat_history: Arc::new(Mutex::new(VecDeque::new())),
            max_contacts,
        }
    }

    /// Create a social context whose new inbox messages are persisted before
    /// delivery. Existing journal rows are restored immediately, before
    /// recipient agents are registered.
    pub fn with_journal(
        max_contacts: usize,
        journal: Option<Arc<crate::inbox_journal::InboxJournal>>,
        timer_journal: Option<Arc<crate::timer_journal::TimeoutJournal>>,
    ) -> crate::Result<Self> {
        Self::with_journals(max_contacts, journal, timer_journal, None)
    }

    /// Create a social context with optional durable delivery and transcript
    /// journals. The transcript survives after inbox rows are consumed.
    pub fn with_journals(
        max_contacts: usize,
        inbox_journal: Option<Arc<crate::inbox_journal::InboxJournal>>,
        timer_journal: Option<Arc<crate::timer_journal::TimeoutJournal>>,
        chat_history_journal: Option<Arc<crate::chat_history_journal::ChatHistoryJournal>>,
    ) -> crate::Result<Self> {
        let journal = inbox_journal;
        let mut inbox_journal = None;
        let inboxes = match journal {
            Some(journal) => {
                inbox_journal = Some(Arc::clone(&journal));
                Arc::new(InboxStore::with_journal(journal)?)
            }
            None => Arc::new(InboxStore::new()),
        };
        let context = Self {
            registry: Arc::new(AgentRegistry::new()),
            inboxes,
            contacts: Arc::new(ContactStore::new()),
            relations: Arc::new(RelationEngine::default()),
            timeouts: Arc::new(TimeoutRegistry::default()),
            timer_journal,
            chat_history_journal,
            monitor_registry: Arc::new(Mutex::new(HashMap::new())),
            controlled_tasks: Arc::new(crate::controlled::ControlledTaskRegistry::default()),
            chat_history: Arc::new(Mutex::new(VecDeque::new())),
            max_contacts,
        };
        if let Some(journal) = &context.timer_journal {
            let manager = crate::timeout_manager::TimeoutManager::new(context.clone());
            for timer in journal.restore()? {
                let request = &timer.request;
                let message_type = match request.kind {
                    crate::timer_journal::TimeoutKind::Timeout => {
                        crate::inbox::MessageType::Timeout
                    }
                    crate::timer_journal::TimeoutKind::SelfReminder => {
                        crate::inbox::MessageType::SelfReminder
                    }
                };
                if let Some(inbox_journal) = &inbox_journal
                    && inbox_journal.has_timer_notification(
                        &request.agent_id,
                        request.task_id.as_deref(),
                        &request.body,
                        message_type,
                    )?
                {
                    journal.ack(timer.id)?;
                    continue;
                }
                manager.restore_timer(timer);
            }
        }
        Ok(context)
    }

    /// Establish a fresh cancellation lifetime for one controlled assignment.
    pub fn begin_controlled_task(&self, task_id: &str) -> tokio_util::sync::CancellationToken {
        self.controlled_tasks.begin(task_id)
    }

    /// Observe the controller-owned lifetime of a controlled assignment.
    pub fn controlled_task_token(
        &self,
        task_id: &str,
    ) -> Option<tokio_util::sync::CancellationToken> {
        self.controlled_tasks.token(task_id)
    }

    /// Invalidate an outstanding controlled assignment and wake its runner.
    pub fn cancel_controlled_task(&self, task_id: &str) -> bool {
        self.controlled_tasks.cancel(task_id)
    }

    /// Mark a task as terminal for controller ingress while retaining its
    /// cancellation token until the runner observes cancellation and exits.
    pub fn retire_controlled_task(&self, task_id: &str) {
        self.controlled_tasks.retire(task_id);
    }

    /// Forget a successfully quiesced controlled assignment.
    pub fn complete_controlled_task(&self, task_id: &str) {
        self.controlled_tasks.complete(task_id);
    }
    /// Register an agent in the registry.
    pub fn register_agent(&self, id: AgentId, expertise: impl Into<String>) {
        let info = AgentInfo {
            id,
            expertise: expertise.into(),
            status: AgentStatus::Idle,
        };
        self.registry.register(info);
    }

    /// Look up an agent's info by ID.
    pub fn get_agent_info(&self, id: &AgentId) -> Option<AgentInfo> {
        self.registry.get(id)
    }
    /// Mark every registered agent as [`AgentStatus::Stopped`].
    ///
    /// Called at team shutdown: statuses left Idle/Busy would misreport
    /// liveness to anyone inspecting the registry afterwards.
    pub fn stop_all(&self) {
        for info in self.registry.list() {
            self.registry.update_status(&info.id, AgentStatus::Stopped);
        }
    }

    /// Track a live monitor task for `agent`.
    ///
    /// Re-registering for the same agent replaces (and aborts) the
    /// previous handle — an agent has at most one live monitor.
    pub(crate) fn register_monitor(&self, agent: &AgentId, handle: AbortHandle) {
        let mut monitors = self.monitor_registry.lock();
        if let Some(old) = monitors.insert(agent.clone(), handle) {
            old.abort();
        }
    }

    /// Remove an agent's monitor entry after the task exits on its own.
    pub(crate) fn unregister_monitor(&self, agent: &AgentId) {
        self.monitor_registry.lock().remove(agent);
    }

    /// Abort every tracked monitor task and clear the registry.
    pub(crate) fn abort_monitors(&self) {
        let mut monitors = self.monitor_registry.lock();
        for (_, handle) in monitors.drain() {
            handle.abort();
        }
    }

    /// Number of currently tracked monitor tasks (test observability).
    pub fn monitor_count(&self) -> usize {
        self.monitor_registry.lock().len()
    }

    /// Update an agent's lifecycle status. Returns `false` if not found.
    pub fn update_status(&self, id: &AgentId, status: AgentStatus) -> bool {
        self.registry.update_status(id, status)
    }

    /// Send a direct message from one agent to another.
    ///
    /// Also ensures the recipient is in the sender's contact list (auto-add
    /// on first communication) and applies a `Communication` relation event
    /// to strengthen the bond. Advances the sender's per-agent tick.
    ///
    /// Delivery goes through [`Self::push_message`], so the recipient also
    /// gains the sender as a contact (inbound auto-contact) — without this,
    /// a recipient who never initiated contact could not reply.
    pub fn send_direct(
        &self,
        from: &AgentId,
        to: &AgentId,
        body: impl Into<String>,
        priority: u32,
    ) -> crate::Result<()> {
        self.send_direct_with_message_id(from, to, body, priority, None, None, None)?;
        Ok(())
    }
    /// Send a direct message with optional `task_id` and `broadcast_id`
    /// correlation keys. Used by the `send_message` tool when replying to
    /// a delegated task or a broadcast.
    ///
    /// Delivery goes through [`Self::push_message`], so the recipient also
    /// gains the sender as a contact (inbound auto-contact) — without this,
    /// a volunteer replying to a broadcast or a delegate replying to a
    /// delegation could be rejected by the `send_message` contact gate.
    pub fn send_direct_with_task(
        &self,
        from: &AgentId,
        to: &AgentId,
        body: impl Into<String>,
        priority: u32,
        task_id: Option<String>,
        broadcast_id: Option<String>,
    ) -> crate::Result<()> {
        self.send_direct_with_task_kind(from, to, body, priority, task_id, broadcast_id, None)?;
        Ok(())
    }

    /// Send a direct message with correlation and optional task lifecycle
    /// metadata. The social transport preserves the kind without interpreting
    /// workflow meaning.
    #[allow(clippy::too_many_arguments)]
    pub fn send_direct_with_task_kind(
        &self,
        from: &AgentId,
        to: &AgentId,
        body: impl Into<String>,
        priority: u32,
        task_id: Option<String>,
        broadcast_id: Option<String>,
        task_kind: Option<TaskMessageKind>,
    ) -> crate::Result<()> {
        self.send_direct_with_message_id_and_task_kind(
            from,
            to,
            body,
            priority,
            task_id,
            broadcast_id,
            None,
            task_kind,
        )?;
        Ok(())
    }

    /// Send a direct message while preserving a caller-provided logical ID.
    ///
    /// Studio APIs use this so the ID emitted over WebSocket is the same ID
    /// later returned by chat history. When no ID is supplied, the social
    /// context generates one before delivery.
    #[allow(clippy::too_many_arguments)]
    pub fn send_direct_with_message_id(
        &self,
        from: &AgentId,
        to: &AgentId,
        body: impl Into<String>,
        priority: u32,
        task_id: Option<String>,
        broadcast_id: Option<String>,
        message_id: Option<String>,
    ) -> crate::Result<String> {
        self.send_direct_with_message_id_and_task_kind(
            from,
            to,
            body,
            priority,
            task_id,
            broadcast_id,
            message_id,
            None,
        )
    }

    /// Full direct-message constructor, including optional task lifecycle
    /// metadata.
    #[allow(clippy::too_many_arguments)]
    pub fn send_direct_with_message_id_and_task_kind(
        &self,
        from: &AgentId,
        to: &AgentId,
        body: impl Into<String>,
        priority: u32,
        task_id: Option<String>,
        broadcast_id: Option<String>,
        message_id: Option<String>,
        task_kind: Option<TaskMessageKind>,
    ) -> crate::Result<String> {
        self.apply_relation_event(from, to, crate::relation::RelationEvent::Communication);
        let message_id = message_id.unwrap_or_else(generate_message_id);
        let message = InboxMessage {
            journal_id: None,
            from: from.clone(),
            to: to.clone(),
            body: body.into(),
            message_type: MessageType::Direct,
            priority,
            task_id,
            task_kind,
            broadcast_id,
            message_id: Some(message_id.clone()),
        };
        self.push_message(message)?;
        Ok(message_id)
    }

    /// Broadcast a message to all agents except the sender.
    ///
    /// Delivery goes through [`Self::push_message`], so every recipient
    /// gains the broadcaster as a contact (inbound auto-contact). Without
    /// this, a volunteer who never contacted the broadcaster could not
    /// reply: the `send_message` contact gate would reject them, and
    /// `search_contacts` only lists their own contacts.
    ///
    /// Returns the generated `broadcast_id` so callers (e.g. the broadcast
    /// tool) can register a cancellable timeout keyed by it.
    pub fn broadcast(&self, from: &AgentId, body: impl Into<String>) -> crate::Result<String> {
        Ok(self
            .broadcast_with_message_id(from, body, None)?
            .broadcast_id)
    }

    /// Broadcast with an explicit logical message ID and return both IDs.
    pub fn broadcast_with_message_id(
        &self,
        from: &AgentId,
        body: impl Into<String>,
        message_id: Option<String>,
    ) -> crate::Result<BroadcastMessageIds> {
        let body = body.into();
        let message_id = message_id.unwrap_or_else(generate_message_id);
        let broadcast_id = uuid::Uuid::now_v7().to_string();
        for info in self.registry.list() {
            if info.id == *from {
                continue;
            }
            let message = InboxMessage {
                journal_id: None,
                from: from.clone(),
                to: info.id.clone(),
                body: body.clone(),
                message_type: MessageType::Broadcast,
                priority: crate::inbox::priority::NORMAL,
                task_id: None,
                task_kind: None,
                broadcast_id: Some(broadcast_id.clone()),
                message_id: Some(message_id.clone()),
            };
            self.push_message(message)?;
        }
        Ok(BroadcastMessageIds {
            message_id,
            broadcast_id,
        })
    }
    /// Push a pre-constructed message into the recipient's inbox.
    ///
    /// Ensures the recipient has the sender in their contact list (inbound
    /// auto-contact): contacts are otherwise only built on the sending side,
    /// which would deadlock replies — a delegate receiving a delegation could
    /// neither `send_message` back (contact gate) nor find the sender via
    /// `search_contacts`. Timer messages (from "timer"/"self") are skipped —
    /// they are system notifications, not social peers.
    pub fn push_message(&self, message: InboxMessage) -> crate::Result<()> {
        let mut message = message;
        if message.message_id.is_none() {
            message.message_id = Some(generate_message_id());
        }
        // A completed pipeline task can never belong to a future waiter.
        // Consume its duplicate/late deliveries at the controller boundary
        // instead of durably enqueueing a row that every later wait would
        // requeue. This is intentionally limited to the private namespace;
        // unknown task IDs are left for the controller to classify.
        if message.to.as_str() == "pipeline-controller"
            && message
                .task_id
                .as_deref()
                .is_some_and(|task_id| task_id.starts_with("task-pipe-"))
            && message
                .task_id
                .as_deref()
                .is_some_and(|task_id| self.controlled_tasks.is_completed(task_id))
        {
            return Ok(());
        }
        if message.from.as_str() != reserved::TIMER
            && message.from.as_str() != reserved::SELF
            && message.from != message.to
        {
            self.ensure_contact(&message.to, &message.from);
        }
        if matches!(
            message.message_type,
            MessageType::Broadcast | MessageType::Direct
        ) {
            if let Some(transcript) = &self.chat_history_journal {
                transcript.append(&message)?;
            }
            let mut history = self.chat_history.lock();
            history.push_back(message.clone());
            while history.len() > 512 {
                history.pop_front();
            }
        }
        self.inboxes.push(message)?;
        Ok(())
    }

    /// Public/group history plus the calling agent's private history.
    pub fn chat_history(&self, agent_id: &AgentId, limit: usize) -> Vec<InboxMessage> {
        let history = self.chat_history.lock();
        history
            .iter()
            .rev()
            .filter(|message| match message.message_type {
                MessageType::Broadcast => true,
                MessageType::Direct => message.from == *agent_id || message.to == *agent_id,
                _ => false,
            })
            .take(limit)
            .cloned()
            .collect()
    }

    /// History for the shared group view, including member replies to a group
    /// broadcast but excluding operator-to-member private conversations.
    pub fn group_history(&self, limit: usize) -> Vec<InboxMessage> {
        let history = self.chat_history.lock();
        let filtered = history
            .iter()
            .rev()
            .filter(|message| match message.message_type {
                MessageType::Broadcast => true,
                MessageType::Direct => {
                    message.broadcast_id.is_some() && message.to.as_str() == reserved::OPERATOR
                }
                _ => false,
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut seen_group_messages = std::collections::HashSet::new();
        filtered
            .into_iter()
            .filter(|message| {
                if message.message_type != MessageType::Broadcast {
                    return true;
                }
                let logical_id = message
                    .message_id
                    .clone()
                    .or_else(|| message.broadcast_id.clone());
                seen_group_messages.insert(logical_id)
            })
            .take(limit)
            .collect()
    }

    /// History for one operator-to-member private conversation.
    pub fn private_history(
        &self,
        peer_a: &AgentId,
        peer_b: &AgentId,
        limit: usize,
    ) -> Vec<InboxMessage> {
        let history = self.chat_history.lock();
        history
            .iter()
            .rev()
            .filter(|message| message.message_type == MessageType::Direct)
            .filter(|message| message.broadcast_id.is_none())
            .filter(|message| {
                (message.from == *peer_a && message.to == *peer_b)
                    || (message.from == *peer_b && message.to == *peer_a)
            })
            .take(limit)
            .cloned()
            .collect()
    }

    /// Durable-aware group history with stable storage sequence.
    pub fn group_history_entries(
        &self,
        limit: usize,
        before: Option<i64>,
    ) -> crate::Result<Vec<ChatHistoryEntry>> {
        if let Some(transcript) = &self.chat_history_journal {
            return transcript
                .group_history(limit, before)
                .map_err(crate::TeamError::from);
        }

        let history = self.chat_history.lock();
        let filtered = history
            .iter()
            .rev()
            .filter(|message| match message.message_type {
                MessageType::Broadcast => true,
                MessageType::Direct => {
                    message.broadcast_id.is_some() && message.to.as_str() == reserved::OPERATOR
                }
                _ => false,
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut seen_group_messages = std::collections::HashSet::new();
        Ok(filtered
            .into_iter()
            .filter(|message| {
                if message.message_type != MessageType::Broadcast {
                    return true;
                }
                let logical_id = message
                    .message_id
                    .clone()
                    .or_else(|| message.broadcast_id.clone());
                seen_group_messages.insert(logical_id)
            })
            .take(limit)
            .map(|message| ChatHistoryEntry {
                sequence: None,
                created_at: None,
                message,
            })
            .collect())
    }

    /// Durable-aware private history with stable storage sequence.
    pub fn private_history_entries(
        &self,
        peer_a: &AgentId,
        peer_b: &AgentId,
        limit: usize,
        before: Option<i64>,
    ) -> crate::Result<Vec<ChatHistoryEntry>> {
        if let Some(transcript) = &self.chat_history_journal {
            return transcript
                .private_history(peer_a, peer_b, limit, before)
                .map_err(crate::TeamError::from);
        }

        let history = self.chat_history.lock();
        Ok(history
            .iter()
            .rev()
            .filter(|message| message.message_type == MessageType::Direct)
            .filter(|message| message.broadcast_id.is_none())
            .filter(|message| {
                (message.from == *peer_a && message.to == *peer_b)
                    || (message.from == *peer_b && message.to == *peer_a)
            })
            .take(limit)
            .cloned()
            .map(|message| ChatHistoryEntry {
                sequence: None,
                created_at: None,
                message,
            })
            .collect())
    }

    /// Return drained messages to an agent's inbox unchanged.
    ///
    /// Used by the monitor task when the harness steer channel is
    /// unavailable: the messages were drained but never consumed, so they
    /// go back to the social inbox for the next run to pick up. Unlike
    /// [`Self::push_message`], no side effects run — the messages already went
    /// through them on first delivery.
    pub fn return_messages(&self, agent_id: &AgentId, messages: Vec<InboxMessage>) {
        for message in messages {
            debug_assert_eq!(&message.to, agent_id, "message returned to wrong inbox");
            self.inboxes.push_returned(message);
        }
    }

    /// Acknowledge durable journal rows after a runner successfully consumed
    /// the associated messages.
    pub fn ack_inbox_messages(&self, messages: &[InboxMessage]) {
        self.inboxes.ack(messages);
    }

    /// Drain all pending messages for clean-shutdown persistence.
    pub fn drain_pending_messages(&self) -> Vec<InboxMessage> {
        self.inboxes.drain_everything()
    }

    /// Restore pending messages after rebuilding the team.
    pub fn restore_pending_messages(&self, messages: Vec<InboxMessage>) {
        self.inboxes.restore(messages);
    }

    /// Drain all pending messages for an agent, returning them in priority
    /// order.
    ///
    /// Messages whose `task_id` or `broadcast_id` matches a **fired**
    /// timeout are silently dropped: the agent has already been nudged
    /// about the timeout and acted on it, so a late reply is stale data
    /// that would only confuse the agent.
    ///
    /// When a Timeout notification itself is drained, its fired mark is
    /// cleared: the mark exists only to filter late replies until the agent
    /// has seen the timeout, so it must not outlive the notification.
    /// SelfReminder notifications never clear marks.
    pub fn drain_inbox(&self, agent_id: &AgentId) -> Vec<InboxMessage> {
        let messages = self.inboxes.drain_all(agent_id);
        let mut dropped = Vec::new();
        let messages = messages
            .into_iter()
            .filter(|msg| {
                // Timeout and self-reminder messages carry the key but are
                // the notifications themselves — never filter them out.
                if matches!(
                    msg.message_type,
                    MessageType::Timeout | MessageType::SelfReminder
                ) {
                    // Consuming the Timeout notification clears its fired
                    // mark (the registry key rides in `task_id`); a
                    // SelfReminder is not a timeout firing — leave marks be.
                    if msg.message_type == MessageType::Timeout
                        && let Some(key) = msg.task_id.as_ref()
                    {
                        self.timeouts.clear_fired(agent_id, key);
                    }
                    return true;
                }
                // Drop late replies whose correlation key has fired.
                let key = msg.task_id.as_ref().or(msg.broadcast_id.as_ref());
                let keep = match key {
                    None => true,
                    Some(k) => !self.is_key_fired(agent_id, k),
                };
                if !keep {
                    dropped.push(msg.clone());
                }
                keep
            })
            .collect();
        self.inboxes.ack(&dropped);
        messages
    }

    /// Ensure a controller-owned pipeline task has a cancellation lifetime
    /// without replacing (and thereby cancelling) an already-live token.
    pub fn ensure_controlled_task(&self, task_id: &str) {
        self.controlled_tasks.ensure(task_id);
    }

    /// Whether an agent has any pending messages.
    pub fn has_messages(&self, agent_id: &AgentId) -> bool {
        self.inboxes.has_messages(agent_id)
    }

    /// Asynchronously wait until at least one message is available.
    pub async fn wait_for_message(&self, agent_id: &AgentId) {
        self.inboxes.wait_for_message(agent_id).await;
    }

    /// List all contacts of an agent.
    pub fn list_contacts(&self, agent: &AgentId) -> Vec<ContactRecord> {
        self.contacts.list_contacts(agent)
    }

    /// Check whether `peer` is a known contact of `agent`.
    pub fn has_contact(&self, agent: &AgentId, peer: &AgentId) -> bool {
        self.contacts.has_contact(agent, peer)
    }

    /// Add a contact for an agent, respecting the configured capacity.
    pub fn add_contact(&self, agent: &AgentId, record: ContactRecord) -> crate::Result<()> {
        self.contacts.add_contact(agent, record, self.max_contacts)
    }

    /// Ensure `peer` is in `agent`'s contact list. If not present, add them
    /// with the initial relationship strength.
    ///
    /// Capacity policy (enforced by [`ContactStore::add_contact`], shared
    /// with [`Self::add_contact`]):
    /// - List below capacity: the peer is added.
    /// - List at capacity and the newcomer at least as strong as the
    ///   weakest incumbent: the weakest incumbent (ties broken toward the
    ///   least-recently interacted) is evicted to make room.
    /// - List at capacity and the newcomer weaker than every incumbent
    ///   (including capacity 0, where nothing can ever be added): the add
    ///   is rejected and the list is left unchanged.
    ///
    /// Rejection is a legitimate policy outcome, not an error: callers
    /// such as [`Self::push_message`] ignore the return value and deliver
    /// the message regardless — only the contact creation is declined, so
    /// no message is lost. Returns `true` if a new contact was created.
    pub fn ensure_contact(&self, agent: &AgentId, peer: &AgentId) -> bool {
        if self.contacts.has_contact(agent, peer) {
            return false;
        }
        let expertise = match self.registry.get(peer) {
            Some(info) => info.expertise.clone(),
            None => String::new(),
        };
        let record = ContactRecord {
            peer_agent_id: peer.clone(),
            peer_expertise: expertise,
            strength: crate::relation::clamp_strength(self.relations.initial_strength()),
            last_interaction_tick: self.contacts.current_tick(agent),
        };
        if self
            .contacts
            .add_contact(agent, record, self.max_contacts)
            .is_ok()
        {
            return true;
        }
        false
    }

    /// Update the strength of a specific contact.
    pub fn update_strength(&self, agent: &AgentId, peer: &AgentId, strength: f64) {
        self.contacts.update_strength(agent, peer, strength);
    }

    /// Apply a relation event to the contact's strength.
    ///
    /// This is the preferred entry point for tools that send messages outside
    /// `send_direct` (e.g. `delegate_task`). It ensures the contact exists,
    /// advances the agent's tick, applies decay to all contacts, and updates
    /// the peer's relation strength.
    pub fn touch_contact(
        &self,
        agent: &AgentId,
        peer: &AgentId,
        event: crate::relation::RelationEvent,
    ) {
        self.apply_relation_event(agent, peer, event);
    }

    /// Apply a relation event to the contact's strength.
    ///
    /// Advances the agent's per-agent tick, ensures the peer exists as a
    /// contact, applies accumulated decay to all contacts, then updates the
    /// contacted peer's `last_interaction_tick` and applies the relation event.
    pub(crate) fn apply_relation_event(
        &self,
        agent: &AgentId,
        peer: &AgentId,
        event: crate::relation::RelationEvent,
    ) {
        let peer_expertise = match self.registry.get(peer) {
            Some(info) => info.expertise.clone(),
            None => String::new(),
        };
        self.contacts.apply_relation_event(
            agent,
            peer,
            &peer_expertise,
            event,
            &self.relations,
            self.max_contacts,
        );
    }

    /// Abort every pending timeout in the registry (team shutdown).
    pub(crate) fn cancel_all_timeouts(&self) {
        let cancelled = self.timeouts.cancel_all();
        for (agent, key) in cancelled {
            self.cancel_timer(&agent, &key);
        }
    }

    /// Number of timer tasks still owned by the shared context.
    pub fn pending_timer_count(&self) -> usize {
        self.timeouts.pending_count()
    }

    pub(crate) fn schedule_timer(
        &self,
        request: &crate::timer_journal::TimeoutRequest,
    ) -> Result<Option<i64>, crate::timer_journal::TimerJournalError> {
        match &self.timer_journal {
            Some(journal) => Ok(Some(journal.schedule(request)?)),
            None => Ok(None),
        }
    }

    pub(crate) fn ack_timer(&self, timer_id: i64) {
        if let Some(journal) = &self.timer_journal
            && let Err(error) = journal.ack(timer_id)
        {
            tracing::error!(timer_id, error = %error, "failed to ack timer journal row");
        }
    }

    pub(crate) fn cancel_timer(&self, agent: &AgentId, key: &str) {
        if let Some(journal) = &self.timer_journal
            && let Err(error) = journal.cancel(agent, key)
        {
            tracing::error!(agent = %agent, key, error = %error, "failed to cancel timer journal row");
        }
    }

    /// The initial relationship strength for a brand-new contact.
    pub fn initial_strength(&self) -> f64 {
        self.relations.initial_strength()
    }

    /// Borrow the relation engine.
    pub fn relation_engine(&self) -> &crate::relation::RelationEngine {
        &self.relations
    }

    /// Register a pending timeout handle under `(agent, key)`.
    ///
    /// `peer` is the remote agent the request targets (`Some` for
    /// delegation timeouts, `None` for broadcasts and self-reminders) —
    /// it is handed back by `cancel`/`mark_fired` for relation events.
    ///
    /// Returns `true` if a pending timeout was replaced under this key.
    pub(crate) fn register_timeout(
        &self,
        agent: &AgentId,
        key: String,
        peer: Option<AgentId>,
        handle: AbortHandle,
    ) -> bool {
        self.timeouts.register(agent, key, peer, handle)
    }

    /// Remove a pending timeout entry without aborting (self-cleanup after
    /// the timeout has fired naturally). Used for self-reminders which
    /// don't need late-reply filtering.
    pub(crate) fn remove_timeout(&self, agent: &AgentId, key: &str) {
        self.timeouts.remove(agent, key);
    }

    /// Mark a timeout as fired. Moves the key from `pending` to `fired`
    /// so that late replies carrying this key are dropped by
    /// [`drain_inbox`]. Used by `set_timeout` (delegate_task / broadcast).
    ///
    /// Returns the peer the timed-out request targeted, if any. `Some(_)`
    /// means a delegation timed out — the collaboration failed — and the
    /// caller emits a `FailedCollaboration` relation event against that
    /// peer. Peer-less timeouts (broadcast, self-reminder) yield `None`.
    pub(crate) fn mark_timeout_fired(&self, agent: &AgentId, key: &str) -> Option<AgentId> {
        self.timeouts.mark_fired(agent, key)
    }

    /// Returns `true` if the timeout for `(agent, key)` has fired —
    /// late replies carrying this key should be dropped.
    pub fn is_key_fired(&self, agent: &AgentId, key: &str) -> bool {
        self.timeouts.has_fired(agent, key)
    }

    /// Abort and remove a pending timeout for `agent` keyed by `key`.
    /// Returns the cancel outcome: `Cancelled { peer: Some(_) }` means a
    /// delegation timeout was cancelled (its reply arrived in time) — the
    /// caller can emit a `SuccessfulCollaboration` relation event toward
    /// that peer. `Cancelled { peer: None }` covers peer-less timeouts
    /// (broadcast, self-reminder); `NotFound` means nothing was pending.
    pub fn cancel_timeout(&self, agent: &AgentId, key: &str) -> TimeoutCancel {
        let outcome = self.timeouts.cancel(agent, key);
        if matches!(outcome, TimeoutCancel::Cancelled { .. }) {
            self.cancel_timer(agent, key);
        }
        outcome
    }

    /// Cancel all pending timeouts for `agent` whose key matches the
    /// `task_id` or `broadcast_id` of any drained message.
    ///
    /// Called by the agent runner after draining the inbox: when a reply
    /// carrying the original task_id/broadcast_id arrives, the corresponding
    /// pending timeout is cancelled so the agent isn't spuriously nudged.
    ///
    /// Cancelling a delegation timeout (peer-carrying) is the collaboration
    /// success signal: a `SuccessfulCollaboration` relation event is applied
    /// from `agent` toward the delegated peer, strengthening that bond.
    /// Broadcast and reminder cancellations emit nothing.
    ///
    /// Ownership: a peer-carrying registration is only cancelled by a reply
    /// from the registered peer (task_id path); peer-less registrations and
    /// broadcast deadlines (UUIDv7 broadcast_id uniqueness) stay unrestricted.
    ///
    /// Returns the identity of every deadline this call cancelled — the
    /// correlation `key` and the targeted `peer` — in message order, so a
    /// later re-arm step can restore a deadline under the same key via
    /// [`SocialContext::re_register_timeout`] (the aborted timers themselves are
    /// dead and not returned; [`TimeoutCancel`] remains available for
    /// single-key explicit cancels). Empty when nothing was cancelled.
    pub fn cancel_timeouts_for_messages(
        &self,
        agent: &AgentId,
        messages: &[InboxMessage],
    ) -> Vec<CancelledTimeout> {
        let mut cancelled = Vec::new();
        for msg in messages {
            // Timeout and SelfReminder messages carry a correlation id but
            // are the notifications themselves, not awaited replies — never
            // cancel a pending timeout on their behalf. (A fired reminder
            // carrying the bare task_id of a still-pending delegate timeout
            // would otherwise abort that timeout and emit a phantom
            // SuccessfulCollaboration toward the silent peer.)
            if matches!(
                msg.message_type,
                MessageType::Timeout | MessageType::SelfReminder
            ) {
                continue;
            }
            if let Some(task_id) = &msg.task_id {
                // Ownership check (task_id path only): a peer-carrying
                // registration may only be cancelled by a reply from that
                // peer — a forged/relayed task_id from another sender must
                // not abort someone else's deadline. Entries registered
                // without a peer (self-reminders) have no owner to enforce
                // and stay cancellable by any sender.
                let owned = match self.timeouts.peer_of(agent, task_id) {
                    // Peer-less registration (broadcast, self-reminder):
                    // no owner to enforce — any sender may cancel.
                    Some(None) => true,
                    // Peer-carrying registration (delegation): only a
                    // reply from the registered peer cancels it.
                    Some(Some(peer)) => msg.from == peer,
                    // No pending entry under this key.
                    None => false,
                };
                if owned
                    && let TimeoutCancel::Cancelled { peer } = self.timeouts.cancel(agent, task_id)
                {
                    self.cancel_timer(agent, task_id);
                    if let Some(peer) = &peer {
                        self.apply_relation_event(
                            agent,
                            peer,
                            crate::relation::RelationEvent::SuccessfulCollaboration,
                        );
                    }
                    cancelled.push(CancelledTimeout {
                        key: task_id.clone(),
                        peer,
                    });
                }
            }
            if let Some(broadcast_id) = &msg.broadcast_id
                && let TimeoutCancel::Cancelled { peer } = self.timeouts.cancel(agent, broadcast_id)
            {
                self.cancel_timer(agent, broadcast_id);
                cancelled.push(CancelledTimeout {
                    key: broadcast_id.clone(),
                    peer,
                });
            }
        }
        cancelled
    }

    /// Restore a previously cancelled or fired deadline under `(agent,
    /// key)` with a live timer's abort handle. The re-arm half of
    /// [`Self::cancel_timeouts_for_messages`]; semantics mirror the
    /// registry's `re_register` (any entry already pending under the key
    /// is aborted and replaced, stale fired marks are cleared).
    ///
    /// Returns `true` if a pending timeout was replaced under this key.
    pub fn re_register_timeout(
        &self,
        agent: &AgentId,
        key: String,
        peer: Option<AgentId>,
        handle: AbortHandle,
    ) -> bool {
        self.timeouts.re_register(agent, key, peer, handle)
    }
}
#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;
    use crate::timeout_manager::reserved;

    #[test]
    fn initial_strength_clamped() {
        // RelationConfig is caller- and file-supplied (Deserialize), so its
        // initial_strength may be out of range. ensure_contact builds new
        // contact records from it directly — the stored strength must still
        // be clamped to [0.0, 1.0] and finite (F8).
        let poisoned = SocialContext {
            registry: Arc::new(AgentRegistry::new()),
            inboxes: Arc::new(InboxStore::new()),
            chat_history: Arc::new(Mutex::new(VecDeque::new())),
            contacts: Arc::new(ContactStore::new()),
            relations: Arc::new(RelationEngine::new(crate::relation::RelationConfig {
                initial_strength: 1.5,
                ..Default::default()
            })),
            timeouts: Arc::new(TimeoutRegistry::default()),
            timer_journal: None,
            monitor_registry: Arc::new(Mutex::new(HashMap::new())),
            controlled_tasks: Arc::new(crate::controlled::ControlledTaskRegistry::default()),
            chat_history_journal: None,
            max_contacts: 10,
        };
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        poisoned.register_agent(alice.clone(), "rust");
        poisoned.register_agent(bob.clone(), "python");

        assert!(poisoned.ensure_contact(&alice, &bob));
        let rec = poisoned.contacts.get_contact(&alice, &bob).unwrap();
        assert_eq!(rec.strength, 1.0, "1.5 must clamp to 1.0");

        // Non-finite config values are invalid, not extreme: they reset to 0.0.
        let nan_ctx = SocialContext {
            relations: Arc::new(RelationEngine::new(crate::relation::RelationConfig {
                initial_strength: f64::NAN,
                ..Default::default()
            })),
            ..poisoned
        };
        let carol = AgentId::new("carol");
        assert!(nan_ctx.ensure_contact(&alice, &carol));
        let rec = nan_ctx.contacts.get_contact(&alice, &carol).unwrap();
        assert_eq!(rec.strength, 0.0, "NaN must reset to 0.0");
    }

    #[test]
    fn register_agent_adds_to_registry() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");

        ctx.register_agent(alice.clone(), "rust systems programming");

        // Registry lookup works.
        let info = ctx
            .get_agent_info(&alice)
            .expect("agent should be registered");
        assert_eq!(info.id, alice);
        assert_eq!(info.expertise, "rust systems programming");
        assert_eq!(info.status, AgentStatus::Idle);

        // Registry list contains the agent.
        let all = ctx.registry.list();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, alice);
        assert_eq!(all[0].expertise, "rust systems programming");
    }

    #[test]
    fn send_message_delivers_to_inbox() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");

        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");

        // Initially no messages.
        assert!(!ctx.has_messages(&bob));

        ctx.send_direct(&alice, &bob, "hello from alice", 5);

        // Bob now has a message.
        assert!(ctx.has_messages(&bob));

        let messages = ctx.drain_inbox(&bob);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, alice);
        assert_eq!(messages[0].to, bob);
        assert_eq!(messages[0].body, "hello from alice");

        // Inbox is drained.
        assert!(!ctx.has_messages(&bob));
    }

    #[test]
    fn broadcast_delivers_to_all() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        let carol = AgentId::new("carol");

        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.register_agent(carol.clone(), "go");

        ctx.broadcast(&alice, "team announcement");

        // Sender (alice) should NOT receive the broadcast.
        assert!(!ctx.has_messages(&alice));

        // All other agents should receive it.
        let bob_msgs = ctx.drain_inbox(&bob);
        assert_eq!(bob_msgs.len(), 1);
        assert_eq!(bob_msgs[0].body, "team announcement");

        let carol_msgs = ctx.drain_inbox(&carol);
        assert_eq!(carol_msgs.len(), 1);
        assert_eq!(carol_msgs[0].body, "team announcement");
    }

    #[test]
    fn chat_messages_have_stable_logical_ids() {
        let ctx = SocialContext::new(10);
        let operator = AgentId::new(reserved::OPERATOR);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(operator.clone(), reserved::OPERATOR);
        ctx.register_agent(alice.clone(), "analysis");
        ctx.register_agent(bob.clone(), "review");

        ctx.send_direct_with_message_id(
            &operator,
            &alice,
            "same text",
            crate::inbox::priority::OPERATOR,
            None,
            None,
            None,
        )
        .unwrap();
        let delivered = ctx.drain_inbox(&alice);
        assert_eq!(delivered.len(), 1);
        let delivered_id = delivered[0].message_id.as_deref();
        let history = ctx.private_history(&operator, &alice, 10);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].message_id.as_deref(), delivered_id);

        let broadcast = ctx
            .broadcast_with_message_id(&operator, "same text", None)
            .unwrap();
        let alice_message = ctx.drain_inbox(&AgentId::new("alice")).remove(0);
        let bob_message = ctx.drain_inbox(&AgentId::new("bob")).remove(0);
        assert_eq!(alice_message.message_id, bob_message.message_id);
        assert_eq!(
            alice_message.broadcast_id.as_deref(),
            Some(broadcast.broadcast_id.as_str())
        );
        assert_eq!(alice_message.message_id, Some(broadcast.message_id));

        ctx.send_direct_with_message_id(
            &bob,
            &operator,
            "same reply",
            0,
            None,
            Some(broadcast.broadcast_id),
            None,
        )
        .unwrap();
        assert_eq!(ctx.group_history(10).len(), 2);
    }

    #[test]
    fn identical_messages_from_different_members_have_distinct_ids() {
        let ctx = SocialContext::new(10);
        let operator = AgentId::new(reserved::OPERATOR);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(operator.clone(), "human");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");

        ctx.send_direct_with_message_id(&alice, &operator, "done", 0, None, None, None)
            .unwrap();
        ctx.send_direct_with_message_id(&bob, &operator, "done", 0, None, None, None)
            .unwrap();

        let alice_message = ctx.private_history(&operator, &alice, 1).remove(0);
        let bob_message = ctx.private_history(&operator, &bob, 1).remove(0);
        assert_ne!(alice_message.message_id, bob_message.message_id);
    }

    #[test]
    fn update_agent_status() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");

        ctx.register_agent(alice.clone(), "rust");

        // Initial status is Idle.
        let info = ctx.get_agent_info(&alice).unwrap();
        assert_eq!(info.status, AgentStatus::Idle);

        // Update to Busy.
        assert!(ctx.update_status(&alice, AgentStatus::Busy));
        let info = ctx.get_agent_info(&alice).unwrap();
        assert_eq!(info.status, AgentStatus::Busy);

        // Update to Stopped.
        assert!(ctx.update_status(&alice, AgentStatus::Stopped));
        let info = ctx.get_agent_info(&alice).unwrap();
        assert_eq!(info.status, AgentStatus::Stopped);

        // Updating a non-existent agent returns false.
        let ghost = AgentId::new("ghost");
        assert!(!ctx.update_status(&ghost, AgentStatus::Busy));
    }

    #[test]
    fn stop_all_marks_members_stopped() {
        // Shutdown must settle member statuses: agents left Idle/Busy after
        // the team is dead misreport liveness to anyone inspecting the
        // registry. stop_all() marks every registered member Stopped.
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");

        // A member mid-run must also end up Stopped, not Busy.
        ctx.update_status(&alice, crate::social_context::AgentStatus::Busy);

        ctx.stop_all();

        let alice_info = ctx.get_agent_info(&alice).unwrap();
        assert_eq!(
            alice_info.status,
            AgentStatus::Stopped,
            "alice must be Stopped after stop_all"
        );
        let bob_info = ctx.get_agent_info(&bob).unwrap();
        assert_eq!(
            bob_info.status,
            AgentStatus::Stopped,
            "bob must be Stopped after stop_all"
        );
    }

    #[test]
    fn tick_advances_on_communication() {
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        let carol = AgentId::new("carol");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.register_agent(carol.clone(), "go");

        // Alice talks to bob (tick 1), then to carol (tick 2).
        ctx.send_direct(&alice, &bob, "hi bob", 0);
        ctx.send_direct(&alice, &carol, "hi carol", 0);

        // Bob's strength should be decayed by 1 tick gap (tick 2 - tick 1 = 1).
        // Initial strength after first comm: initial + communication_delta.
        // After second comm decay: strength - decay_per_tick * 1.
        let bob_rec = ctx
            .contacts
            .get_contact(&alice, &bob)
            .expect("bob should be a contact");
        let carol_rec = ctx
            .contacts
            .get_contact(&alice, &carol)
            .expect("carol should be a contact");

        // Carol was just contacted at tick 2 — no decay.
        // Bob was contacted at tick 1, decayed 1 tick at tick 2.
        let engine = ctx.relation_engine();
        let expected_bob = engine.apply_decay(
            engine.config().initial_strength + engine.config().communication_delta,
            1,
        );
        assert!(
            (bob_rec.strength - expected_bob).abs() < 1e-9,
            "bob strength {} should be {} (decayed 1 tick)",
            bob_rec.strength,
            expected_bob
        );

        // Carol has no decay.
        let expected_carol = engine.config().initial_strength + engine.config().communication_delta;
        assert!(
            (carol_rec.strength - expected_carol).abs() < 1e-9,
            "carol strength {} should be {} (no decay)",
            carol_rec.strength,
            expected_carol
        );
    }

    #[test]
    fn per_agent_tick_independent() {
        // Alice and Bob each have their own tick. Alice talks to Bob twice
        // (alice tick → 2). Bob never initiates communication, so Bob's tick
        // stays 0. If we then have Bob talk to Carol, Bob's tick advances to 1
        // — not 3 (Alice's activity must not leak into Bob's tick).
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        let carol = AgentId::new("carol");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.register_agent(carol.clone(), "go");

        // Alice's two communications advance alice's tick to 2.
        ctx.send_direct(&alice, &bob, "hi bob", 0);
        ctx.send_direct(&alice, &bob, "hi again", 0);
        assert_eq!(ctx.contacts.current_tick(&alice), 2);

        // Bob has not initiated any communication — his tick is 0.
        assert_eq!(ctx.contacts.current_tick(&bob), 0);

        // Bob initiates a message to Carol. His tick advances to 1 (not 3).
        ctx.send_direct(&bob, &carol, "hi carol from bob", 0);
        assert_eq!(ctx.contacts.current_tick(&bob), 1);

        // Alice's tick is unchanged by Bob's activity.
        assert_eq!(ctx.contacts.current_tick(&alice), 2);
    }

    #[tokio::test]
    async fn timeout_registry_cancel_aborts_pending_timeout() {
        use std::time::Duration;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "test");

        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());
        let _handle = timer.set_timeout(
            alice.clone(),
            "follow up",
            Duration::from_secs(60),
            Some("task-0".to_string()),
            Some(AgentId::new("bob")),
        );

        // The timeout is registered under (alice, "task-0"); cancelling
        // returns the recorded peer so a reply-driven cancel can emit a
        // SuccessfulCollaboration relation event.
        assert_eq!(
            ctx.cancel_timeout(&alice, "task-0"),
            TimeoutCancel::Cancelled {
                peer: Some(AgentId::new("bob"))
            }
        );
        // A second cancel finds nothing (already removed).
        assert_eq!(
            ctx.cancel_timeout(&alice, "task-0"),
            TimeoutCancel::NotFound
        );
    }

    #[tokio::test]
    async fn cancel_timeouts_for_messages_cancels_matching_task_id() {
        use std::time::Duration;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "test");
        ctx.register_agent(bob.clone(), "test");
        ctx.ensure_contact(&alice, &bob);

        // Alice sets a timeout keyed by task-0.
        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());
        timer.set_timeout(
            alice.clone(),
            "follow up task-0",
            Duration::from_secs(60),
            Some("task-0".to_string()),
            None,
        );

        // Bob replies with a Direct message carrying task_id = "task-0".
        ctx.send_direct_with_task(&bob, &alice, "done", 0, Some("task-0".to_string()), None);

        // Alice drains her inbox — the reply should cancel the timeout.
        let messages = ctx.drain_inbox(&alice);
        ctx.cancel_timeouts_for_messages(&alice, &messages);

        // The timeout was cancelled: a subsequent explicit cancel finds nothing.
        assert_eq!(
            ctx.cancel_timeout(&alice, "task-0"),
            TimeoutCancel::NotFound
        );
    }

    #[tokio::test]
    async fn cancel_rejects_timeout_registered_by_other_peer() {
        use std::time::Duration;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        let carol = AgentId::new("carol");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.register_agent(carol.clone(), "go");

        // Alice delegates to Bob: the timeout is registered with peer=Bob.
        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());
        timer.set_timeout(
            alice.clone(),
            "follow up task-0",
            Duration::from_secs(60),
            Some("task-0".to_string()),
            Some(bob.clone()),
        );

        // Carol sends a reply carrying the same task_id — a forged or
        // relayed correlation id from a non-registered sender.
        ctx.send_direct_with_task(&carol, &alice, "done", 0, Some("task-0".to_string()), None);
        let messages = ctx.drain_inbox(&alice);
        ctx.cancel_timeouts_for_messages(&alice, &messages);

        // Nothing was cancelled: the deadline is still pending under
        // (alice, task-0) with its registered peer.
        assert_eq!(
            ctx.cancel_timeout(&alice, "task-0"),
            TimeoutCancel::Cancelled {
                peer: Some(bob.clone())
            }
        );

        // Bob's own reply (the registered peer) does cancel it.
        ctx.send_direct_with_task(&bob, &alice, "done", 0, Some("task-0".to_string()), None);
        let messages = ctx.drain_inbox(&alice);
        ctx.cancel_timeouts_for_messages(&alice, &messages);
        assert_eq!(
            ctx.cancel_timeout(&alice, "task-0"),
            TimeoutCancel::NotFound
        );
    }

    #[tokio::test]
    async fn cancel_returns_identity_triple() {
        use std::time::Duration;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        // Two pending deadlines: a delegation (peer-carrying) and a
        // peer-less broadcast-style one.
        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());
        timer.set_timeout(
            alice.clone(),
            "follow up task-0",
            Duration::from_secs(60),
            Some("task-0".to_string()),
            Some(bob.clone()),
        );
        timer.set_timeout(
            alice.clone(),
            "follow up broadcast",
            Duration::from_secs(60),
            Some("b-1".to_string()),
            None,
        );

        // One reply cancels the delegation deadline, another the
        // broadcast one.
        ctx.send_direct_with_task(&bob, &alice, "done", 0, Some("task-0".to_string()), None);
        ctx.send_direct_with_task(&bob, &alice, "ack", 0, None, Some("b-1".to_string()));
        let messages = ctx.drain_inbox(&alice);
        let cancelled = ctx.cancel_timeouts_for_messages(&alice, &messages);

        // The returned identities let a re-arm flow re-register each
        // deadline: (key, peer) pairs must match the original
        // registrations.
        assert_eq!(cancelled.len(), 2);
        let mut keys: Vec<&str> = cancelled.iter().map(|c| c.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["b-1", "task-0"]);
        let task0 = cancelled.iter().find(|c| c.key == "task-0").unwrap();
        assert_eq!(task0.peer, Some(bob.clone()));
        let b1 = cancelled.iter().find(|c| c.key == "b-1").unwrap();
        assert_eq!(b1.peer, None);

        // Both deadlines are really gone from the registry.
        assert_eq!(
            ctx.cancel_timeout(&alice, "task-0"),
            TimeoutCancel::NotFound
        );
        assert_eq!(ctx.cancel_timeout(&alice, "b-1"), TimeoutCancel::NotFound);
    }

    #[tokio::test]
    async fn re_register_restores_deadline() {
        use std::time::Duration;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        // Deadline registered with a peer fires naturally: the Timeout
        // message is delivered and the key moves to the fired set.
        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());
        let handle = timer.set_timeout(
            alice.clone(),
            "follow up task-0",
            Duration::from_millis(20),
            Some("task-0".to_string()),
            Some(bob.clone()),
        );
        handle.await.unwrap();
        assert!(ctx.is_key_fired(&alice, "task-0"));

        // Drain the notification (this clears the fired mark, see
        // drain_inbox), then force a stale fired mark back onto the key:
        // re_register must clear it so the restored deadline can fire —
        // and filter late replies — again.
        let drained = ctx.drain_inbox(&alice);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].message_type, MessageType::Timeout);
        ctx.timeouts.mark_fired(&alice, "task-0");
        assert!(ctx.is_key_fired(&alice, "task-0"));

        // Restore the deadline under the same correlation key with a
        // fresh live timer.
        let resumed = tokio::spawn({
            let ctx = ctx.clone();
            let alice = alice.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                ctx.push_message(InboxMessage {
                    journal_id: None,
                    from: AgentId::new(crate::timeout_manager::reserved::TIMER),
                    to: alice.clone(),
                    body: "follow up task-0".to_string(),
                    message_type: MessageType::Timeout,
                    priority: crate::inbox::priority::TIMEOUT,
                    task_id: Some("task-0".to_string()),
                    task_kind: None,
                    broadcast_id: None,
                    message_id: None,
                });
                let _peer = ctx.mark_timeout_fired(&alice, "task-0");
            }
        });
        let replaced = ctx.re_register_timeout(
            &alice,
            "task-0".to_string(),
            Some(bob.clone()),
            resumed.abort_handle(),
        );
        assert!(!replaced, "no pending entry existed after the fire");

        // The restored entry carries the registered peer, is pending
        // again, and its stale fired mark is gone.
        assert_eq!(
            ctx.timeouts.peer_of(&alice, "task-0"),
            Some(Some(bob.clone()))
        );
        assert!(!ctx.is_key_fired(&alice, "task-0"));

        // The restored timer really delivers its Timeout message on fire,
        // and consuming it clears the fired mark (late replies keyed
        // "task-0" flow again).
        resumed.await.unwrap();
        assert!(ctx.is_key_fired(&alice, "task-0"));
        let msgs = ctx.drain_inbox(&alice);
        assert_eq!(msgs.len(), 1, "re-registered deadline must deliver");
        assert_eq!(msgs[0].message_type, MessageType::Timeout);
        assert_eq!(msgs[0].task_id.as_deref(), Some("task-0"));
        assert!(!ctx.is_key_fired(&alice, "task-0"));
    }

    #[tokio::test]
    async fn re_register_replaces_pending_entry_and_cancel_path_preserves_identity() {
        use std::time::Duration;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        // The literal E7 flow: a pending deadline is cancelled via the
        // message path; the returned identity is re-armed under the same
        // key with a live timer.
        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());
        timer.set_timeout(
            alice.clone(),
            "follow up task-0",
            Duration::from_secs(60),
            Some("task-0".to_string()),
            Some(bob.clone()),
        );
        ctx.send_direct_with_task(&bob, &alice, "done", 0, Some("task-0".to_string()), None);
        let messages = ctx.drain_inbox(&alice);
        let cancelled = ctx.cancel_timeouts_for_messages(&alice, &messages);
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].key, "task-0");
        assert_eq!(cancelled[0].peer, Some(bob.clone()));

        // Re-register from the cancelled identity: the deadline is
        // pending again with its peer.
        let resumed = tokio::spawn({
            let ctx = ctx.clone();
            let alice = alice.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                ctx.push_message(InboxMessage {
                    journal_id: None,
                    from: AgentId::new(crate::timeout_manager::reserved::TIMER),
                    to: alice.clone(),
                    body: "follow up task-0".to_string(),
                    message_type: MessageType::Timeout,
                    priority: crate::inbox::priority::TIMEOUT,
                    task_id: Some("task-0".to_string()),
                    task_kind: None,
                    broadcast_id: None,
                    message_id: None,
                });
                let _peer = ctx.mark_timeout_fired(&alice, "task-0");
            }
        });
        let fresh = ctx.re_register_timeout(
            &alice,
            cancelled[0].key.clone(),
            cancelled[0].peer.clone(),
            resumed.abort_handle(),
        );
        assert!(!fresh);
        assert_eq!(
            ctx.timeouts.peer_of(&alice, "task-0"),
            Some(Some(bob.clone()))
        );

        // Re-registering over a still-pending entry replaces it and
        // aborts the previous timer.
        let second = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let replaced =
            ctx.re_register_timeout(&alice, "task-0".to_string(), None, second.abort_handle());
        assert!(replaced);
        // The first re-armed timer must have been aborted — its fire
        // task never completes; drop its JoinHandle.
        drop(resumed);
        assert_eq!(ctx.timeouts.peer_of(&alice, "task-0"), Some(None));

        // The replacement is a real entry: an explicit cancel aborts it.
        assert_eq!(
            ctx.cancel_timeout(&alice, "task-0"),
            TimeoutCancel::Cancelled { peer: None }
        );
        // And the aborted first timer never fires: a short yield finds
        // no Timeout message in alice's inbox.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(ctx.drain_inbox(&alice).is_empty());
    }

    #[tokio::test]
    async fn cancel_allows_no_peer_registration_cancelled_by_any() {
        use std::time::Duration;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");

        // Alice sets a timeout WITHOUT a peer (self-reminder style: only
        // she is accountable for it).
        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());
        timer.set_timeout(
            alice.clone(),
            "check results",
            Duration::from_secs(60),
            Some("self-check".to_string()),
            None,
        );

        // Any reply carrying the key cancels it — a peer-less registration
        // has no owner to enforce.
        ctx.send_direct_with_task(
            &bob,
            &alice,
            "done",
            0,
            Some("self-check".to_string()),
            None,
        );
        let messages = ctx.drain_inbox(&alice);
        ctx.cancel_timeouts_for_messages(&alice, &messages);

        assert_eq!(
            ctx.cancel_timeout(&alice, "self-check"),
            TimeoutCancel::NotFound
        );
    }

    #[tokio::test]
    async fn timely_delegation_reply_strengthens_bond() {
        use std::time::Duration;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());
        timer.set_timeout(
            alice.clone(),
            "follow up task-0",
            Duration::from_secs(60),
            Some("task-0".to_string()),
            Some(bob.clone()),
        );

        // Bob replies in time carrying the task_id.
        ctx.send_direct_with_task(&bob, &alice, "done", 0, Some("task-0".to_string()), None);
        let messages = ctx.drain_inbox(&alice);
        ctx.cancel_timeouts_for_messages(&alice, &messages);
        // On alice's side the bond only receives SuccessfulCollaboration
        // (+0.15): the reply's Communication event lands on bob's index
        // (sender side). The relation event first advances alice's tick
        // to 1 and settles 1 tick of decay on the untouched contact:
        // (0.1 - 0.001) + 0.15 = 0.249.
        let engine = ctx.relation_engine();
        let expected = engine.initial_strength() - engine.config().decay_per_tick
            + engine.config().collaboration_success_delta;
        let strength = ctx.contacts.get_contact(&alice, &bob).unwrap().strength;
        assert!(
            (strength - expected).abs() < 1e-9,
            "timely reply must strengthen the bond, got {strength}"
        );
    }

    #[tokio::test]
    async fn delegation_timeout_fire_weakens_bond() {
        use std::time::Duration;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());
        let handle = timer.set_timeout(
            alice.clone(),
            "task-0 timed out",
            Duration::from_millis(10),
            Some("task-0".to_string()),
            Some(bob.clone()),
        );
        handle.await.unwrap();
        // The fire path advances alice's tick to 1, settling 1 tick of
        // decay before applying FailedCollaboration (-0.05):
        // (0.1 - 0.001) - 0.05 = 0.049.
        let engine = ctx.relation_engine();
        let expected = engine.initial_strength() - engine.config().decay_per_tick
            + engine.config().collaboration_failure_delta;
        let strength = ctx.contacts.get_contact(&alice, &bob).unwrap().strength;
        assert!(
            (strength - expected).abs() < 1e-9,
            "timeout fire must weaken the bond, got {strength}"
        );
    }

    #[tokio::test]
    async fn drain_inbox_delivers_late_reply_alongside_timeout_notification() {
        use std::time::Duration;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        // Alice delegates task-0 to bob with a short timeout.
        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());
        let handle = timer.set_timeout(
            alice.clone(),
            "task-0 timed out",
            Duration::from_millis(10),
            Some("task-0".to_string()),
            None,
        );
        handle.await.unwrap();

        // Bob replies late, before alice drains anything.
        ctx.send_direct_with_task(
            &bob,
            &alice,
            "sorry, done now",
            0,
            Some("task-0".to_string()),
            None,
        );

        // One drain delivers the notification AND the late reply: the
        // Timeout notification (higher priority) is filtered first and
        // clears the fired mark, so the reply is no longer stale. (B1:
        // the mark must not outlive the notification.)
        let msgs = ctx.drain_inbox(&alice);
        assert_eq!(msgs.len(), 2, "notification and late reply both delivered");
        assert_eq!(msgs[0].message_type, MessageType::Timeout);
        assert_eq!(msgs[1].body, "sorry, done now");
        assert!(!ctx.is_key_fired(&alice, "task-0"));
    }

    #[tokio::test]
    async fn drain_inbox_delivers_late_broadcast_reply_alongside_notification() {
        use std::time::Duration;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());
        // Alice broadcasts with a short timeout, keyed by broadcast_id "bc-0".
        let handle = timer.set_timeout(
            alice.clone(),
            "broadcast bc-0 timed out",
            Duration::from_millis(10),
            Some("bc-0".to_string()),
            None,
        );
        handle.await.unwrap();

        // Bob volunteers late, before alice drains anything.
        ctx.send_direct_with_task(
            &bob,
            &alice,
            "I can help",
            0,
            None,
            Some("bc-0".to_string()),
        );

        // One drain delivers both: the Timeout notification clears the
        // fired mark, so the late volunteer reply is no longer stale.
        let msgs = ctx.drain_inbox(&alice);
        assert_eq!(msgs.len(), 2, "notification and late reply both delivered");
        assert_eq!(msgs[0].message_type, MessageType::Timeout);
        assert_eq!(msgs[1].body, "I can help");
        assert!(!ctx.is_key_fired(&alice, "bc-0"));
    }

    #[tokio::test]
    async fn drain_inbox_keeps_replies_before_timeout_fires() {
        use std::time::Duration;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);
        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());

        // Alice delegates task-0 with a long timeout.
        timer.set_timeout(
            alice.clone(),
            "task-0 timed out",
            Duration::from_secs(60),
            Some("task-0".to_string()),
            None,
        );

        // Bob replies promptly.
        ctx.send_direct_with_task(&bob, &alice, "done", 0, Some("task-0".to_string()), None);

        // Alice drains — the reply should be present (timeout hasn't fired).
        let msgs = ctx.drain_inbox(&alice);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "done");
    }

    #[test]
    fn timeout_fired_mark_cleared_after_drain() {
        // B1: a fired timeout's mark must not outlive its notification.
        // A stale mark makes later handling of the same key misbehave as
        // "already fired" (late replies silently dropped forever).
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        // Force-fire a timeout for "task-0" (the mark_fired path).
        ctx.timeouts.mark_fired(&alice, "task-0");
        assert!(ctx.is_key_fired(&alice, "task-0"));

        // Put the resulting Timeout notification into the queue.
        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new(reserved::TIMER),
            to: alice.clone(),
            body: "task-0 timed out".into(),
            message_type: MessageType::Timeout,
            priority: crate::inbox::priority::TIMEOUT,
            task_id: Some("task-0".to_string()),
            task_kind: None,
            broadcast_id: None,
            message_id: None,
        });

        // Draining the notification clears the fired mark.
        let msgs = ctx.drain_inbox(&alice);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].message_type, MessageType::Timeout);
        assert!(
            !ctx.is_key_fired(&alice, "task-0"),
            "draining the Timeout notification must clear the fired mark"
        );
    }

    #[test]
    fn self_reminder_does_not_clear_fired() {
        // SelfReminder notifications are not timeout firings: draining one
        // must leave any fired mark (e.g. for a still-relevant delegation
        // key) intact, preserving late-reply filtering.
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        ctx.register_agent(alice.clone(), "rust");

        // Force-fire a mark for "task-0".
        ctx.timeouts.mark_fired(&alice, "task-0");
        assert!(ctx.is_key_fired(&alice, "task-0"));

        // A SelfReminder notification (NOT a Timeout) is drained.
        ctx.push_message(InboxMessage {
            journal_id: None,
            from: AgentId::new(reserved::SELF),
            to: alice.clone(),
            body: "check on bob".into(),
            message_type: MessageType::SelfReminder,
            priority: crate::inbox::priority::SELF_REMINDER,
            task_id: Some("task-0".to_string()),
            task_kind: None,
            broadcast_id: None,
            message_id: None,
        });
        let msgs = ctx.drain_inbox(&alice);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].message_type, MessageType::SelfReminder);
        assert!(
            ctx.is_key_fired(&alice, "task-0"),
            "SelfReminder drain must not clear the fired mark"
        );
    }

    #[tokio::test]
    async fn late_reply_delivered_after_timeout_notification_drained() {
        // End-to-end B1 behavior: once the Timeout notification itself has
        // been drained, a late reply for the same key is no longer stale —
        // it must be delivered instead of silently dropped.
        use std::time::Duration;

        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());
        let handle = timer.set_timeout(
            alice.clone(),
            "task-0 timed out",
            Duration::from_millis(10),
            Some("task-0".to_string()),
            None,
        );
        handle.await.unwrap();
        assert!(ctx.is_key_fired(&alice, "task-0"));

        // Drain the Timeout notification — this clears the fired mark.
        let msgs = ctx.drain_inbox(&alice);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].message_type, MessageType::Timeout);
        assert!(!ctx.is_key_fired(&alice, "task-0"));

        // Bob replies late — now deliverable again.
        ctx.send_direct_with_task(
            &bob,
            &alice,
            "sorry, done now",
            0,
            Some("task-0".to_string()),
            None,
        );
        let msgs = ctx.drain_inbox(&alice);
        assert_eq!(
            msgs.len(),
            1,
            "late reply must be delivered after the notification was drained"
        );
        assert_eq!(msgs[0].body, "sorry, done now");
    }
    #[test]
    fn push_message_auto_adds_sender_to_recipient_contacts() {
        // Inbound auto-contact: without this, a delegate receiving a
        // delegation could neither reply (contact gate in send_message)
        // nor find the sender via search_contacts.
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");

        // Alice sends bob a delegation request. Bob has never contacted
        // alice — his contact list must still gain her as a contact.
        let message = InboxMessage {
            journal_id: None,
            from: alice.clone(),
            to: bob.clone(),
            body: "do this task".into(),
            message_type: MessageType::DelegationRequest,
            priority: crate::inbox::priority::DELEGATION,
            task_id: Some("task-alice-0".into()),
            task_kind: None,
            broadcast_id: None,
            message_id: None,
        };
        ctx.push_message(message);

        assert!(ctx.has_contact(&bob, &alice));
        assert!(!ctx.has_contact(&alice, &bob)); // sender side untouched here

        // Timer messages are system notifications — no social contact.
        let timer_msg = InboxMessage {
            journal_id: None,
            from: AgentId::new(reserved::TIMER),
            to: alice.clone(),
            body: "timed out".into(),
            message_type: MessageType::Timeout,
            priority: crate::inbox::priority::TIMEOUT,
            task_id: Some("task-alice-0".into()),
            task_kind: None,
            broadcast_id: None,
            message_id: None,
        };
        ctx.push_message(timer_msg);
        assert!(!ctx.has_contact(&alice, &AgentId::new(reserved::TIMER)));
    }

    #[test]
    fn broadcast_auto_adds_broadcaster_to_recipient_contacts() {
        // Volunteer-reply path: a recipient who never contacted the
        // broadcaster must still gain them as a contact, or the
        // send_message contact gate rejects their reply and
        // search_contacts cannot find the broadcaster either.
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");

        ctx.broadcast(&alice, "who can help with parsing?");

        // Bob received the broadcast and gained alice as a contact.
        assert!(ctx.has_contact(&bob, &alice));
        // Alice (the sender) did not add recipients on her side.
        assert!(!ctx.has_contact(&alice, &bob));
        // Bob got the message.
        let msgs = ctx.drain_inbox(&bob);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].message_type, MessageType::Broadcast);
    }

    #[test]
    fn send_direct_auto_adds_sender_to_recipient_contacts() {
        // Inbound auto-contact for direct messages: a recipient who never
        // initiated contact must still gain the sender, or the send_message
        // contact gate rejects their reply and search_contacts can't find
        // the sender either. Mirrors push_message/broadcast behavior.
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");

        ctx.send_direct(&alice, &bob, "analyze this", 5);

        // Bob gained alice as a contact; alice's own send-side auto-add is
        // handled by apply_relation_event (sender-side).
        assert!(ctx.has_contact(&bob, &alice));
        // Bob got the message with sender attribution intact.
        let msgs = ctx.drain_inbox(&bob);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, alice);
        assert_eq!(msgs[0].body, "analyze this");
    }

    #[test]
    fn ensure_contact_evicts_weakest_when_full() {
        // Capacity pressure: a newcomer at least as strong as the weakest
        // incumbent evicts that incumbent (weakest-victim policy). Pins
        // the eviction so a regression to silent add-failure is caught.
        let ctx = SocialContext::new(2);
        let agent = AgentId::new("agent");
        ctx.register_agent(agent.clone(), "rust");
        let initial = ctx.initial_strength();
        for (name, strength) in [("strong", 0.9), ("weak", initial / 2.0)] {
            ctx.add_contact(
                &agent,
                ContactRecord {
                    peer_agent_id: AgentId::new(name),
                    peer_expertise: "x".to_string(),
                    strength,
                    last_interaction_tick: 0,
                },
            )
            .unwrap();
        }

        // The newcomer arrives at the initial strength — at least as
        // strong as the weakened incumbent, so the weakest one is evicted
        // to make room.
        assert!(ctx.ensure_contact(&agent, &AgentId::new("newcomer")));
        assert!(ctx.has_contact(&agent, &AgentId::new("newcomer")));
        assert!(
            !ctx.has_contact(&agent, &AgentId::new("weak")),
            "the weakest incumbent must be the eviction victim"
        );
        // Non-victims survive; capacity is still respected.
        assert!(ctx.has_contact(&agent, &AgentId::new("strong")));
        assert_eq!(ctx.list_contacts(&agent).len(), 2);
    }

    #[test]
    fn ensure_contact_rejects_weaker_than_all_incumbents() {
        // Policy: a full list with a newcomer weaker than every incumbent
        // adds no value — the add is rejected and the list is left
        // unchanged. ensure_contact reports this as `false`; nothing is
        // evicted.
        let ctx = SocialContext::new(2);
        let agent = AgentId::new("agent");
        ctx.register_agent(agent.clone(), "rust");
        let initial = ctx.initial_strength();
        for (name, strength) in [("a", initial + 0.1), ("b", initial + 0.2)] {
            ctx.add_contact(
                &agent,
                ContactRecord {
                    peer_agent_id: AgentId::new(name),
                    peer_expertise: "x".to_string(),
                    strength,
                    last_interaction_tick: 0,
                },
            )
            .unwrap();
        }

        assert!(!ctx.ensure_contact(&agent, &AgentId::new("newcomer")));
        // No eviction happened: both incumbents survive, list unchanged.
        assert!(ctx.has_contact(&agent, &AgentId::new("a")));
        assert!(ctx.has_contact(&agent, &AgentId::new("b")));
        assert!(!ctx.has_contact(&agent, &AgentId::new("newcomer")));
        assert_eq!(ctx.list_contacts(&agent).len(), 2);
    }

    #[test]
    fn ensure_contact_zero_capacity_rejects() {
        // Capacity 0 rejects every add: there is no incumbent to evict.
        let ctx = SocialContext::new(0);
        let agent = AgentId::new("agent");
        ctx.register_agent(agent.clone(), "rust");

        assert!(!ctx.ensure_contact(&agent, &AgentId::new("peer")));
        assert!(ctx.list_contacts(&agent).is_empty());
    }

    #[test]
    fn push_message_still_delivers_when_contact_creation_is_rejected() {
        // ensure_contact failures at capacity are policy rejections, not
        // delivery failures: push_message ignores the contact result and
        // enqueues unconditionally, so no message is lost.
        let ctx = SocialContext::new(2);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        let initial = ctx.initial_strength();
        // Bob's contact list is full of stronger incumbents.
        for (name, strength) in [("a", initial + 0.1), ("b", initial + 0.2)] {
            ctx.add_contact(
                &bob,
                ContactRecord {
                    peer_agent_id: AgentId::new(name),
                    peer_expertise: "x".to_string(),
                    strength,
                    last_interaction_tick: 0,
                },
            )
            .unwrap();
        }

        ctx.push_message(InboxMessage {
            journal_id: None,
            from: alice.clone(),
            to: bob.clone(),
            body: "urgent request".into(),
            message_type: MessageType::Direct,
            priority: crate::inbox::priority::NORMAL,
            task_id: None,
            task_kind: None,
            broadcast_id: None,
            message_id: None,
        });

        // Delivered despite the rejected inbound auto-contact.
        let msgs = ctx.drain_inbox(&bob);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, alice);
        assert_eq!(msgs[0].body, "urgent request");
        // Only the contact creation was declined.
        assert!(!ctx.has_contact(&bob, &alice));
    }

    #[tokio::test]
    async fn register_timeout_overwrite_aborts_previous_timer() {
        // Re-registering the same (agent, key) aborts the previous timer
        // task, so an LLM reusing a task_id across two set_reminder calls
        // cannot produce duplicate SelfReminder deliveries.
        let ctx = SocialContext::new(10);
        let agent = AgentId::new("carol");
        use std::time::Duration;
        let tm = crate::timeout_manager::TimeoutManager::new(ctx.clone());

        let first = tm.set_self_reminder(
            agent.clone(),
            "first reminder",
            Duration::from_millis(50),
            Some("same-key".to_string()),
        );
        assert!(
            !first,
            "first registration on a fresh key is not a replacement"
        );
        let second = tm.set_self_reminder(
            agent.clone(),
            "second reminder",
            Duration::from_millis(50),
            Some("same-key".to_string()),
        );
        assert!(
            second,
            "second registration must report it replaced the first timer"
        );

        // Exactly one reminder is delivered.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let msgs = ctx.drain_inbox(&agent);
        let reminders: Vec<_> = msgs
            .iter()
            .filter(|m| m.message_type == MessageType::SelfReminder)
            .collect();
        assert_eq!(reminders.len(), 1, "duplicate SelfReminder delivery");
        assert_eq!(reminders[0].body, "second reminder");
    }
    #[tokio::test]
    async fn fired_set_evicts_oldest_key_at_capacity() {
        // The fired set is bounded. Once MAX_FIRED_KEYS_PER_AGENT is
        // reached, the oldest key is evicted: a late reply for an evicted
        // key is delivered (not dropped), while a late reply for a still-
        // remembered key stays filtered.
        let ctx = SocialContext::new(10);
        let alice = AgentId::new("alice");
        let bob = AgentId::new("bob");
        ctx.register_agent(alice.clone(), "rust");
        ctx.register_agent(bob.clone(), "python");
        ctx.ensure_contact(&alice, &bob);

        // Fire one timeout for "key-0000".
        let timer = crate::timeout_manager::TimeoutManager::new(ctx.clone());
        let handle = timer.set_timeout(
            alice.clone(),
            "timed out",
            std::time::Duration::from_millis(1),
            Some("key-0000".to_string()),
            None,
        );
        handle.await.unwrap();
        assert!(ctx.is_key_fired(&alice, "key-0000"));
        // Deliver and discard the timeout notification itself. Under B1
        // this clears the fired mark, so re-mark "key-0000" directly —
        // this test exercises the eviction bound, not mark lifetime.
        let msgs = ctx.drain_inbox(&alice);
        assert_eq!(msgs.len(), 1);
        ctx.timeouts.mark_fired(&alice, "key-0000");

        // Flood the fired set with MAX distinct keys, evicting key-0000.
        for i in 0..crate::social_context::MAX_FIRED_KEYS_PER_AGENT {
            let k = format!("filler-{i:04}");
            ctx.timeouts.mark_fired(&alice, &k);
        }
        assert!(
            !ctx.is_key_fired(&alice, "key-0000"),
            "oldest fired key must have been evicted"
        );
        // A still-remembered filler key keeps filtering late replies.
        ctx.send_direct_with_task(
            &bob,
            &alice,
            "late reply for fresh key",
            0,
            Some("filler-1023".to_string()),
            None,
        );
        assert!(
            ctx.drain_inbox(&alice).is_empty(),
            "reply for remembered fired key must stay dropped"
        );

        // A late reply for the evicted key is delivered again.
        ctx.send_direct_with_task(
            &bob,
            &alice,
            "very late reply",
            0,
            Some("key-0000".to_string()),
            None,
        );
        let msgs = ctx.drain_inbox(&alice);
        assert_eq!(msgs.len(), 1, "reply for evicted key must be delivered");
        assert_eq!(msgs[0].body, "very late reply");
    }
}
