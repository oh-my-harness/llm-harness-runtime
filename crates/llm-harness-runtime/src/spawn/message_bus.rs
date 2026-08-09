//! Unified message bus for main↔sub agent communication.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use llm_harness_agent::{AgentHarness, AgentHarnessEvent};
use llm_harness_types::{AgentError, AgentMessage, ChannelError, CustomMessage};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::spawn::spawn_types::{SubAgentHandle, SubAgentResult};

pub(crate) const EVENT_CHANNEL_CAPACITY: usize = 256;

/// The conventional agent ID for the main agent in the `MessageBus` registry.
/// Tools like `MessageSubagentTool` and `AwaitSubagentReplyTool` use this as
/// their `main_id`, and track tasks look up waiters under this key.
pub const MAIN_AGENT_ID: &str = "Main";

/// Sub-agent event delivered to the main agent via the event channel.
#[derive(Debug, Clone)]
pub enum SubAgentEvent {
    /// Sub-agent completed (success/failure/abort).
    Done {
        agent_id: String,
        result: Result<SubAgentResult, AgentError>,
    },
    /// Sub-agent sent an intermediate message (sub→main, no waiter).
    Message { agent_id: String, body: String },
}

/// Agent status in the registry.
#[derive(Debug, Clone)]
pub enum AgentStatus {
    Running,
    Done(Box<SubAgentResult>),
    Aborted,
}

/// Snapshot of an agent's status for query_agent_status.
#[derive(Debug, Clone)]
pub struct AgentStatusSnapshot {
    pub agent_id: String,
    pub status: AgentStatus,
    /// 子 Agent 的角色标签（来自 SpawnOptions.role）。纯展示元数据。
    pub role: Option<String>,
    /// 子 Agent 的职责描述（来自 SpawnOptions.description）。纯展示元数据。
    pub description: Option<String>,
}

/// Entry in the agent registry.
pub struct AgentEntry {
    pub status: AgentStatus,
    /// Sub agent harness reference. Some while running, None after detach.
    /// Main agent is always None (harness held externally).
    pub harness: Option<Arc<AgentHarness>>,
    pub is_main: bool,
    /// 子 Agent 的角色标签（来自 SpawnOptions.role）。纯展示元数据。
    pub role: Option<String>,
    /// 子 Agent 的职责描述（来自 SpawnOptions.description）。纯展示元数据。
    pub description: Option<String>,
}

impl AgentEntry {
    /// Detach the harness reference (called when sub agent completes).
    pub fn detach(&mut self) {
        self.harness = None;
    }

    /// Build a status snapshot from this entry.
    pub fn to_snapshot(&self, agent_id: &str) -> AgentStatusSnapshot {
        AgentStatusSnapshot {
            agent_id: agent_id.to_string(),
            status: self.status.clone(),
            role: self.role.clone(),
            description: self.description.clone(),
        }
    }
}

/// Send receipt from MessageBus::send.
#[derive(Debug)]
pub enum SendReceipt {
    /// Delivered to a waiter or injected via steer/event channel.
    Injected,
    /// Agent not found or terminated.
    Failed(String),
}

/// Waiter for a specific agent's message.
struct Waiter {
    from: Option<String>,
    tx: oneshot::Sender<SubAgentEvent>,
}

/// RAII guard for borrowing the event_rx. Drop returns rx to the bus.
pub struct EventRxGuard {
    rx: Option<mpsc::Receiver<SubAgentEvent>>,
    registry: Arc<MessageBus>,
}

impl EventRxGuard {
    pub fn try_recv(&mut self) -> Option<SubAgentEvent> {
        self.rx.as_mut()?.try_recv().ok()
    }

    pub async fn recv(&mut self) -> Option<SubAgentEvent> {
        self.rx.as_mut()?.recv().await
    }
}

impl Drop for EventRxGuard {
    fn drop(&mut self) {
        if let Some(rx) = self.rx.take() {
            self.registry.return_event_rx(rx);
        }
    }
}

/// Unified message bus: event channel + agent registry + waiter table.
pub struct MessageBus {
    event_tx: mpsc::Sender<SubAgentEvent>,
    event_rx: Mutex<Option<mpsc::Receiver<SubAgentEvent>>>,
    in_flight: Arc<AtomicUsize>,
    agents: Mutex<HashMap<String, AgentEntry>>,
    waiters: Mutex<HashMap<String, Vec<Waiter>>>,
    subagent_aborts: Mutex<HashMap<String, CancellationToken>>,
    /// Set by IdleWatcher when the idle timeout fires. Consumed (swap to
    /// false) by `should_stop` to inject a one-shot status hint so the LLM
    /// knows sub-agents are still running.
    idle_timeout_fired: AtomicBool,
}

impl MessageBus {
    /// Create a new MessageBus with a fresh event channel.
    pub fn new() -> (Arc<Self>, mpsc::Receiver<SubAgentEvent>) {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let bus = Arc::new(Self {
            event_tx: tx,
            event_rx: Mutex::new(None), // rx held externally for test; production uses take_event_rx
            in_flight: Arc::new(AtomicUsize::new(0)),
            agents: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
            subagent_aborts: Mutex::new(HashMap::new()),
            idle_timeout_fired: AtomicBool::new(false),
        });
        (bus, rx)
    }

    /// Create for tests: returns bus plus a separate receiver for test
    /// assertions. event_rx is left as None (test uses the returned rx
    /// directly, not via take_event_rx).
    #[cfg(test)]
    fn new_for_test() -> (Self, mpsc::Receiver<SubAgentEvent>) {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let bus = Self {
            event_tx: tx,
            event_rx: Mutex::new(None),
            in_flight: Arc::new(AtomicUsize::new(0)),
            agents: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
            subagent_aborts: Mutex::new(HashMap::new()),
            idle_timeout_fired: AtomicBool::new(false),
        };
        (bus, rx)
    }

    /// Register an agent.
    pub fn register(&self, id: &str, entry: AgentEntry) {
        self.agents.lock().insert(id.to_string(), entry);
    }

    /// Update an agent's status.
    pub fn update_status(&self, id: &str, status: AgentStatus) {
        if let Some(entry) = self.agents.lock().get_mut(id) {
            entry.status = status;
        }
    }

    /// Query agent status snapshots. None = all agents.
    pub fn query_status(&self, id: Option<&str>) -> Vec<AgentStatusSnapshot> {
        let agents = self.agents.lock();
        match id {
            Some(id) => agents
                .get(id)
                .map(|e| vec![e.to_snapshot(id)])
                .unwrap_or_default(),
            None => agents.iter().map(|(id, e)| e.to_snapshot(id)).collect(),
        }
    }

    /// Subscribe to a sub agent's harness event broadcast.
    /// Returns None if agent not found or harness detached.
    /// TUI uses this to observe sub agent events (tool calls, text deltas, etc).
    pub fn subscribe_agent(
        &self,
        id: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<Arc<AgentHarnessEvent>>> {
        let agents = self.agents.lock();
        agents
            .get(id)
            .and_then(|e| e.harness.as_ref().map(|h| h.subscribe()))
    }

    /// Detach an agent's harness reference (called by track task on completion).
    pub fn detach_agent(&self, id: &str) {
        if let Some(entry) = self.agents.lock().get_mut(id) {
            entry.detach();
        }
    }

    /// Remove an agent from the registry entirely (called by track task
    /// after completion event has been delivered).
    pub(crate) fn remove_agent(&self, id: &str) {
        self.agents.lock().remove(id);
    }

    /// Fire-and-forget message send.
    pub async fn send(&self, from: &str, to: &str, body: &str) -> SendReceipt {
        // 1. Check for matching waiter (separate lock from agents).
        let waiter_match = {
            let mut waiters = self.waiters.lock();
            if let Some(list) = waiters.get_mut(to) {
                let idx = list
                    .iter()
                    .position(|w| w.from.is_none() || w.from.as_deref() == Some(from));
                if let Some(idx) = idx {
                    let waiter = list.remove(idx);
                    if list.is_empty() {
                        waiters.remove(to);
                    }
                    Some(waiter)
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some(waiter) = waiter_match {
            // Deliver via oneshot — does not go through steer or event channel.
            // If tx.send() fails, the waiter's rx was dropped (timeout/abort in
            // progress). Fall through to steer/event-channel delivery below.
            let msg = SubAgentEvent::Message {
                agent_id: from.to_string(),
                body: body.to_string(),
            };
            if waiter.tx.send(msg).is_ok() {
                return SendReceipt::Injected;
            }
        }

        // 2. No waiter — route by agent type. Check status and steer
        //    inside the agents lock to avoid TOCTOU: without holding the
        //    lock, the track task could set status=Done + detach between
        //    our status check and steer_message call.
        let msg_for_sub = AgentMessage::Custom(CustomMessage {
            r#type: "main_message".into(),
            data: serde_json::json!({"from": from, "body": body}),
            timestamp: chrono::Utc::now(),
        });

        let (is_main, steer_result) = {
            let agents = self.agents.lock();
            match agents.get(to) {
                Some(e) => match &e.status {
                    AgentStatus::Aborted => {
                        return SendReceipt::Failed(format!("Agent '{to}' is aborted"));
                    }
                    AgentStatus::Done(_) => {
                        return SendReceipt::Failed(format!("Agent '{to}' has completed"));
                    }
                    _ => {
                        if e.is_main {
                            (true, Ok(()))
                        } else if let Some(h) = &e.harness {
                            (false, h.steer_message(msg_for_sub))
                        } else {
                            // Running but harness already detached — treat as completed.
                            return SendReceipt::Failed(format!("Agent '{to}' harness detached"));
                        }
                    }
                },
                None => return SendReceipt::Failed(format!("Agent '{to}' not found")),
            }
        };

        // 3. Main agent: send to event channel (should_stop / IdleWatcher).
        if is_main {
            let _ = self
                .event_tx
                .send(SubAgentEvent::Message {
                    agent_id: from.to_string(),
                    body: body.to_string(),
                })
                .await;
            return SendReceipt::Injected;
        }

        // 4. Sub agent: report steer result.
        match steer_result {
            Ok(()) => SendReceipt::Injected,
            Err(reason) => SendReceipt::Failed(format!("Agent '{to}': {reason}")),
        }
    }

    /// Block until a message arrives for `agent_id`, optionally filtered by `from`.
    /// `timeout: None` means wait indefinitely (only abort can interrupt).
    /// Returns None on timeout or abort.
    pub async fn wait(
        &self,
        agent_id: &str,
        filter_from: Option<&str>,
        timeout: Option<Duration>,
        abort: CancellationToken,
    ) -> Option<SubAgentEvent> {
        let (tx, rx) = oneshot::channel::<SubAgentEvent>();
        let waiter = Waiter {
            from: filter_from.map(String::from),
            tx,
        };
        {
            let mut waiters = self.waiters.lock();
            waiters
                .entry(agent_id.to_string())
                .or_default()
                .push(waiter);
        }

        let result = match timeout {
            None => tokio::select! {
                msg = rx => msg.ok(),
                _ = abort.cancelled() => None,
            },
            Some(d) => tokio::select! {
                msg = rx => msg.ok(),
                _ = tokio::time::sleep(d) => None,
                _ = abort.cancelled() => None,
            },
        };
        // select! drops the incomplete branch futures, so on the timeout/abort
        // path rx (oneshot::Receiver) has already been dropped — tx.is_closed()
        // returns true for our waiter and the retain() below removes it.
        // On the msg=Err path (sender dropped without sending), msg.ok() returns
        // None (instead of using `?` which would early-return and skip cleanup).

        // Clean up waiter if still registered (timeout/abort/recv-error path).
        if result.is_none() {
            let mut waiters = self.waiters.lock();
            if let Some(list) = waiters.get_mut(agent_id) {
                list.retain(|w| !w.tx.is_closed());
                if list.is_empty() {
                    waiters.remove(agent_id);
                }
            }
        }

        result
    }

    // ── Event channel management ──

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }
    /// Atomically reserve an in-flight slot (unlimited).
    /// The caller must call `release_slot` if the spawn fails before
    /// `track_with_oneshot` consumes the slot.
    pub(crate) fn acquire_slot(&self) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    /// Atomically reserve an in-flight slot if under `max`.
    /// Returns `false` if the limit is already reached.
    pub(crate) fn try_acquire_slot(&self, max: usize) -> bool {
        loop {
            let current = self.in_flight.load(Ordering::Relaxed);
            if current >= max {
                return false;
            }
            if self
                .in_flight
                .compare_exchange(current, current + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Release a slot acquired by `acquire_slot` or `try_acquire_slot`
    /// when the spawn fails before `track_with_oneshot` consumes it.
    pub(crate) fn release_slot(&self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    /// Called by IdleWatcher when the idle timeout fires.
    pub(crate) fn set_idle_timeout_fired(&self) {
        self.idle_timeout_fired.store(true, Ordering::Relaxed);
    }

    /// Called by `should_stop`: consumes the flag (swap to false).
    /// Returns `true` if the idle timeout had fired since the last call.
    pub(crate) fn consume_idle_timeout_fired(&self) -> bool {
        self.idle_timeout_fired.swap(false, Ordering::Relaxed)
    }

    pub fn take_event_rx(self: &Arc<Self>) -> Option<EventRxGuard> {
        let mut guard = self.event_rx.lock();
        guard.take().map(|rx| EventRxGuard {
            rx: Some(rx),
            registry: self.clone(),
        })
    }

    fn return_event_rx(&self, rx: mpsc::Receiver<SubAgentEvent>) {
        let mut guard = self.event_rx.lock();
        *guard = Some(rx);
    }

    pub(crate) fn cancel_all(&self) {
        // Iterate and cancel — do NOT drain (mem::take). The AbortCascadeHook
        // contract requires re-callable: harness.abort() may be invoked again
        // after re-prompt. CancellationToken::cancel() is idempotent, so
        // cancelling already-cancelled tokens is harmless.
        let aborts = self.subagent_aborts.lock();
        for token in aborts.values() {
            token.cancel();
        }
    }
    /// Try to re-send an event back to the event channel (non-blocking).
    /// Used by IdleWatcher to put a received event back so should_stop can drain it.
    pub(crate) fn try_resend_event(&self, event: SubAgentEvent) -> Result<(), ChannelError> {
        self.event_tx.try_send(event).map_err(ChannelError::from)
    }

    /// Re-send an event back to the event channel (async, blocks if full).
    /// Used by IdleWatcher to put a received event back so should_stop can drain it.
    pub(crate) async fn resend_event(&self, event: SubAgentEvent) {
        let _ = self.event_tx.send(event).await;
    }

    pub(crate) fn register_subagent_abort(&self, agent_id: &str, token: CancellationToken) {
        self.subagent_aborts
            .lock()
            .insert(agent_id.to_string(), token);
    }

    pub(crate) fn unregister_subagent_abort(&self, agent_id: &str) {
        self.subagent_aborts.lock().remove(agent_id);
    }

    #[cfg(test)]
    pub(crate) fn registered_abort_count(&self) -> usize {
        self.subagent_aborts.lock().len()
    }

    /// Cancel a specific sub-agent's abort token (if registered).
    pub fn abort_agent(&self, id: &str) {
        if let Some(token) = self.subagent_aborts.lock().get(id) {
            token.cancel();
        }
    }

    /// Track a sub-agent: spawn a background task to join the handle.
    /// On completion: update status=Done, send SubAgentEvent::Done
    /// (via waiter if matched, else to event channel).
    #[cfg(test)]
    pub(crate) fn track(self: &Arc<Self>, handle: SubAgentHandle, agent_id: String) -> String {
        let abort = handle.abort_token();
        let event_tx = self.event_tx.clone();
        let in_flight = self.in_flight.clone();
        let id = agent_id.clone();
        let registry = self.clone();

        in_flight.fetch_add(1, Ordering::Relaxed);
        self.subagent_aborts
            .lock()
            .insert(id.clone(), abort.clone());

        let id_for_status = id.clone();
        let id_for_remove = id.clone();
        tokio::spawn(async move {
            let result = handle.join().await;
            let status = match &result {
                Ok(r) => AgentStatus::Done(Box::new(r.clone())),
                Err(AgentError::Aborted) => AgentStatus::Aborted,
                Err(_) => AgentStatus::Aborted,
            };
            registry.update_status(&id_for_status, status.clone());
            registry.detach_agent(&id_for_status);

            let event = SubAgentEvent::Done {
                agent_id: id.clone(),
                result,
            };

            // Try to match a waiter first.
            // 1. Check for a waiter registered under this agent's ID.
            // 2. Check for a waiter registered under MAIN_AGENT_ID with a `from`
            //    filter matching this agent's ID (AwaitSubagentReplyTool).
            let waiter_match = {
                let mut waiters = registry.waiters.lock();
                let mut matched = None;
                if let Some(list) = waiters.get_mut(&id)
                    && let Some(idx) = list
                        .iter()
                        .position(|w| w.from.is_none() || w.from.as_deref() == Some(&id))
                {
                    let waiter = list.remove(idx);
                    if list.is_empty() {
                        waiters.remove(&id);
                    }
                    matched = Some(waiter);
                }
                if matched.is_none()
                    && let Some(list) = waiters.get_mut(MAIN_AGENT_ID)
                    && let Some(idx) = list.iter().position(|w| w.from.as_deref() == Some(&id))
                {
                    let waiter = list.remove(idx);
                    if list.is_empty() {
                        waiters.remove(MAIN_AGENT_ID);
                    }
                    matched = Some(waiter);
                }
                matched
            };
            // Try oneshot delivery first. send() consumes `event` and returns
            // Err(t) if the waiter's rx was dropped (timeout/abort) — in that
            // case fall back to the event channel so the Done event is not lost.
            let event = match waiter_match {
                Some(waiter) => match waiter.tx.send(event) {
                    Ok(()) => {
                        registry.subagent_aborts.lock().remove(&id_for_remove);
                        registry.remove_agent(&id_for_remove);
                        in_flight.fetch_sub(1, Ordering::Relaxed);
                        return;
                    }
                    Err(e) => e,
                },
                None => event,
            };
            let _ = event_tx.send(event).await;

            registry.subagent_aborts.lock().remove(&id_for_remove);
            registry.remove_agent(&id_for_remove);
            in_flight.fetch_sub(1, Ordering::Relaxed);
        });

        agent_id
    }

    /// Track a sub-agent, returning an AsyncSpawnHandle.
    /// On completion: update status=Done, deliver SubAgentEvent::Done
    /// (via waiter if matched, else to event channel).
    pub(crate) fn track_with_oneshot(
        self: &Arc<Self>,
        handle: SubAgentHandle,
        agent_id: String,
    ) -> crate::spawn::delivery::AsyncSpawnHandle {
        let abort = handle.abort_token();
        let event_tx = self.event_tx.clone();
        let id = agent_id.clone();
        let registry = self.clone();

        // in_flight is pre-incremented by `try_acquire_slot` in `spawn_async`
        // before this method is called, so the concurrency limit is enforced
        // atomically. `track` (test-only) does its own fetch_add.
        self.subagent_aborts
            .lock()
            .insert(id.clone(), abort.clone());

        let id_for_status = id.clone();
        let id_for_remove = id.clone();
        tokio::spawn(async move {
            let result = handle.join().await;
            let status = match &result {
                Ok(r) => AgentStatus::Done(Box::new(r.clone())),
                Err(AgentError::Aborted) => AgentStatus::Aborted,
                Err(_) => AgentStatus::Aborted,
            };
            registry.update_status(&id_for_status, status);
            registry.detach_agent(&id_for_status);

            let event = SubAgentEvent::Done {
                agent_id: id.clone(),
                result,
            };

            // Try waiter match first, else event channel.
            // 1. Check for a waiter registered under this agent's ID
            //    (e.g. sub-agent awaiting a message from itself).
            // 2. Check for a waiter registered under MAIN_AGENT_ID with a `from`
            //    filter matching this agent's ID (AwaitSubagentReplyTool).
            let waiter_match = {
                let mut waiters = registry.waiters.lock();
                // Step 1: match under the sub-agent's own ID.
                let mut matched = None;
                if let Some(list) = waiters.get_mut(&id)
                    && let Some(idx) = list
                        .iter()
                        .position(|w| w.from.is_none() || w.from.as_deref() == Some(&id))
                {
                    let waiter = list.remove(idx);
                    if list.is_empty() {
                        waiters.remove(&id);
                    }
                    matched = Some(waiter);
                }
                // Step 2: match under MAIN_AGENT_ID with from=Some(sub_agent_id).
                if matched.is_none()
                    && let Some(list) = waiters.get_mut(MAIN_AGENT_ID)
                    && let Some(idx) = list.iter().position(|w| w.from.as_deref() == Some(&id))
                {
                    let waiter = list.remove(idx);
                    if list.is_empty() {
                        waiters.remove(MAIN_AGENT_ID);
                    }
                    matched = Some(waiter);
                }
                matched
            };
            // Try oneshot delivery first. send() consumes `event` and returns
            // Err(t) if the waiter's rx was dropped (timeout/abort) — in that
            // case fall back to the event channel so the Done event is not lost.
            let event = match waiter_match {
                Some(waiter) => match waiter.tx.send(event) {
                    Ok(()) => {
                        registry.subagent_aborts.lock().remove(&id_for_remove);
                        registry.remove_agent(&id_for_remove);
                        registry.in_flight.fetch_sub(1, Ordering::Relaxed);
                        return;
                    }
                    Err(e) => e, // rx dropped, fall back to event channel
                },
                None => event,
            };
            let _ = event_tx.send(event).await;

            registry.subagent_aborts.lock().remove(&id_for_remove);
            registry.remove_agent(&id_for_remove);
            registry.in_flight.fetch_sub(1, Ordering::Relaxed);
        });

        crate::spawn::delivery::AsyncSpawnHandle::new(agent_id, abort)
    }
}

/// Create a paired MessageBus with the event_rx stored internally so that
/// `take_event_rx` works in production. The caller retrieves the receiver
/// later via `bus.take_event_rx()` (typically wrapped in an `EventRxGuard`).
pub fn message_bus_pair() -> Arc<MessageBus> {
    let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    Arc::new(MessageBus {
        event_tx: tx,
        event_rx: Mutex::new(Some(rx)),
        in_flight: Arc::new(AtomicUsize::new(0)),
        agents: Mutex::new(HashMap::new()),
        waiters: Mutex::new(HashMap::new()),
        subagent_aborts: Mutex::new(HashMap::new()),
        idle_timeout_fired: AtomicBool::new(false),
    })
}

impl llm_harness_agent::AbortCascadeHook for MessageBus {
    fn cancel_all_subagents(&self) {
        self.cancel_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::HarnessBuilder;
    use llm_harness_loop::LlmClient;
    use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};
    use llm_harness_types::ExecutionEnv;
    use std::time::Duration;

    /// Build a real (lightweight) AgentHarness via HarnessBuilder + MockLlmClient.
    async fn make_harness() -> Arc<AgentHarness> {
        let client: Arc<dyn LlmClient> =
            Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
        Arc::new(
            HarnessBuilder::new("test-model")
                .provider("test-model", client)
                .build(env)
                .await
                .expect("build harness"),
        )
    }

    #[tokio::test]
    async fn send_to_agent_with_waiter_delivers_via_oneshot() {
        let (bus, _event_rx) = MessageBus::new_for_test();
        let bus = Arc::new(bus);

        // Register a sub agent with a real harness.
        let harness = make_harness().await;
        bus.register(
            "sub-1",
            AgentEntry {
                status: AgentStatus::Running,
                harness: Some(harness),
                is_main: false,
                role: None,
                description: None,
            },
        );

        // Spawn a waiter
        let bus_clone = bus.clone();
        let wait_task = tokio::spawn(async move {
            bus_clone
                .wait(
                    "sub-1",
                    None,
                    Some(Duration::from_secs(5)),
                    CancellationToken::new(),
                )
                .await
        });

        // Give waiter time to register
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send a message — should match the waiter
        let receipt = bus.send("main", "sub-1", "hello sub").await;
        assert!(matches!(receipt, SendReceipt::Injected));

        let event = wait_task.await.unwrap().expect("should receive message");
        match event {
            SubAgentEvent::Message { agent_id, body } => {
                assert_eq!(agent_id, "main");
                assert_eq!(body, "hello sub");
            }
            _ => panic!("expected Message event"),
        }
    }

    #[tokio::test]
    async fn send_to_sub_without_waiter_steers_message() {
        let (bus, _event_rx) = MessageBus::new_for_test();
        let bus = Arc::new(bus);

        let harness = make_harness().await;
        bus.register(
            "sub-1",
            AgentEntry {
                status: AgentStatus::Running,
                harness: Some(harness.clone()),
                is_main: false,
                role: None,
                description: None,
            },
        );

        let receipt = bus.send("main", "sub-1", "steer this").await;
        assert!(
            matches!(receipt, SendReceipt::Failed(_)),
            "send to sub with closed steer channel should return Failed, got {receipt:?}"
        );
        // The harness's steer channel receiver is dropped at construction
        // (only reset_channels() during prompt() creates a live pair).
        // steer_message returns Err(Closed), and send() correctly reports
        // Failed rather than falsely claiming delivery.
    }

    #[tokio::test]
    async fn send_to_main_without_waiter_sends_event() {
        let (bus, mut event_rx) = MessageBus::new_for_test();
        let bus = Arc::new(bus);

        bus.register(
            MAIN_AGENT_ID,
            AgentEntry {
                status: AgentStatus::Running,
                harness: None,
                is_main: true,
                role: None,
                description: None,
            },
        );

        let receipt = bus.send("sub-1", MAIN_AGENT_ID, "report from sub").await;
        assert!(matches!(receipt, SendReceipt::Injected));

        let event = event_rx.recv().await.expect("should receive event");
        match event {
            SubAgentEvent::Message { agent_id, body } => {
                assert_eq!(agent_id, "sub-1");
                assert_eq!(body, "report from sub");
            }
            _ => panic!("expected Message event"),
        }
    }

    #[tokio::test]
    async fn send_to_unknown_agent_returns_failed() {
        let (bus, _event_rx) = MessageBus::new_for_test();
        let bus = Arc::new(bus);

        let receipt = bus.send("main", "nonexistent", "hello").await;
        assert!(matches!(receipt, SendReceipt::Failed(_)));
    }

    #[tokio::test]
    async fn query_status_returns_all_agents() {
        let (bus, _event_rx) = MessageBus::new_for_test();
        let bus = Arc::new(bus);

        bus.register(
            MAIN_AGENT_ID,
            AgentEntry {
                status: AgentStatus::Running,
                harness: None,
                is_main: true,
                role: None,
                description: None,
            },
        );
        bus.register(
            "sub-1",
            AgentEntry {
                status: AgentStatus::Running,
                harness: None,
                is_main: false,
                role: None,
                description: None,
            },
        );

        let snapshot = bus.query_status(None);
        assert_eq!(snapshot.len(), 2);
    }

    #[tokio::test]
    async fn wait_times_out_returns_none() {
        let (bus, _event_rx) = MessageBus::new_for_test();
        let bus = Arc::new(bus);

        bus.register(
            "sub-1",
            AgentEntry {
                status: AgentStatus::Running,
                harness: None,
                is_main: false,
                role: None,
                description: None,
            },
        );

        let result = bus
            .wait(
                "sub-1",
                None,
                Some(Duration::from_millis(50)),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn cancel_all_aborts_all_running_subagents() {
        let (bus, _rx) = MessageBus::new();
        let bus = Arc::new(bus);

        let token1 = CancellationToken::new();
        let token2 = CancellationToken::new();

        bus.register(
            "sub-1",
            AgentEntry {
                status: AgentStatus::Running,
                harness: None,
                is_main: false,
                role: None,
                description: None,
            },
        );
        bus.register_subagent_abort("sub-1", token1.clone());
        bus.register_subagent_abort("sub-2", token2.clone());

        bus.cancel_all();

        assert!(token1.is_cancelled());
        assert!(token2.is_cancelled());
    }

    #[tokio::test]
    async fn wait_timeout_then_send_does_not_match_stale_waiter() {
        // Fix 1: after wait() times out, the waiter entry must be removed so
        // a subsequent send() does not match a stale oneshot and lose the
        // message. With a sub-agent (non-main, harness=None), send() should
        // return Failed (harness detached) — NOT match a dead waiter.
        let (bus, _event_rx) = MessageBus::new_for_test();
        let bus = Arc::new(bus);

        bus.register(
            "sub-1",
            AgentEntry {
                status: AgentStatus::Running,
                harness: None,
                is_main: false,
                role: None,
                description: None,
            },
        );

        // wait() times out — returns None.
        let result = bus
            .wait(
                "sub-1",
                None,
                Some(Duration::from_millis(20)),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_none());

        // After timeout, no waiter should remain.
        {
            let waiters = bus.waiters.lock();
            assert!(
                waiters.get("sub-1").is_none(),
                "stale waiter survived timeout cleanup"
            );
        }

        // send() must NOT match a stale waiter. With harness=None and
        // is_main=false, it returns Failed (harness detached).
        // The key assertion: no panic, no lost message via a dead oneshot.
        let receipt = bus.send("main", "sub-1", "after-timeout").await;
        assert!(matches!(receipt, SendReceipt::Failed(_)));
    }

    #[tokio::test]
    async fn send_to_done_agent_returns_failed() {
        // Fix 2: sending to a Done sub-agent must return Failed, not
        // silently return Injected (false success).
        let (bus, _event_rx) = MessageBus::new_for_test();
        let bus = Arc::new(bus);

        let done_result = SubAgentResult {
            final_answer: "done".into(),
            tool_calls_count: 0,
            session_id: "s1".into(),
            cost: llm_harness_types::CostAggregate::default(),
        };
        bus.register(
            "sub-1",
            AgentEntry {
                status: AgentStatus::Done(Box::new(done_result)),
                harness: None,
                is_main: false,
                role: None,
                description: None,
            },
        );

        let receipt = bus.send("main", "sub-1", "hello").await;
        assert!(
            matches!(receipt, SendReceipt::Failed(_)),
            "sending to Done agent should fail, got {receipt:?}"
        );
    }

    #[tokio::test]
    async fn message_bus_pair_stores_event_rx_internally() {
        // Fix 3: message_bus_pair() must store the event receiver in event_rx
        // so take_event_rx() returns Some in production.
        let bus = message_bus_pair();
        let guard = bus
            .take_event_rx()
            .expect("take_event_rx should return Some");
        // Dropping the guard returns rx to the bus.
        drop(guard);
        // Can take again after return.
        let _guard2 = bus
            .take_event_rx()
            .expect("take_event_rx should work after guard drop");
    }

    #[tokio::test]
    async fn abort_agent_cancels_registered_token() {
        // Fix 4: abort_agent cancels a specific sub-agent's token.
        let (bus, _rx) = MessageBus::new();
        let bus = Arc::new(bus);

        let token = CancellationToken::new();
        bus.register_subagent_abort("sub-1", token.clone());

        assert!(!token.is_cancelled());
        bus.abort_agent("sub-1");
        assert!(token.is_cancelled());

        // Aborting an unknown agent is a no-op (no panic).
        bus.abort_agent("nonexistent");
    }
}
