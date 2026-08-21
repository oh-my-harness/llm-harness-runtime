//! Unified message bus for main↔sub agent communication.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use llm_harness_agent::{AgentHarness, AgentHarnessEvent};
use llm_harness_types::{AgentError, AgentMessage, CustomMessage};
use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::spawn_types::{SubAgentHandle, SubAgentResult};

/// The conventional agent ID for the main agent in the `MessageBus` registry.
/// Tools like `MessageSubagentTool` and `AwaitSubagentReplyTool` use this as
/// their `main_id`, and track tasks look up waiters under this key.
pub const MAIN_AGENT_ID: &str = "Main";

/// Sub-agent event delivered to the main agent via the event sink callback.
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
    /// Delivered to a waiter or injected via steer/event sink.
    Injected,
    /// Agent not found or terminated.
    Failed(String),
}

/// Waiter for a specific agent's message.
struct Waiter {
    from: Option<String>,
    tx: oneshot::Sender<SubAgentEvent>,
}

type EventSink = Arc<dyn Fn(SubAgentEvent) + Send + Sync>;

/// Unified message bus: event sink + agent registry + waiter table.
pub struct MessageBus {
    event_sink: Mutex<Option<EventSink>>,
    in_flight: Arc<AtomicUsize>,
    agents: Mutex<HashMap<String, AgentEntry>>,
    waiters: Mutex<HashMap<String, Vec<Waiter>>>,
    subagent_aborts: Mutex<HashMap<String, CancellationToken>>,
    idle_timeout_fired: AtomicBool,
}

impl MessageBus {
    /// Create a new MessageBus with no event sink. Main agent events are
    /// silently dropped until `set_event_sink` is called (by SpawnPlugin).
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            event_sink: Mutex::new(None),
            in_flight: Arc::new(AtomicUsize::new(0)),
            agents: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
            subagent_aborts: Mutex::new(HashMap::new()),
            idle_timeout_fired: AtomicBool::new(false),
        })
    }

    /// Set the event sink callback. Called by `SpawnPlugin::new` to route
    /// main-agent events (sub-agent completions, sub→main messages) into
    /// the plugin's internal queue.
    pub fn set_event_sink(&self, sink: EventSink) {
        *self.event_sink.lock() = Some(sink);
    }

    /// Push an event to waiters (if matched) or the event sink (if set).
    /// Called by track task (fallback when no waiter matched) and `send()`
    /// for main-agent delivery.
    ///
    /// Waiter matching here covers the race where `send()` is called after
    /// the track task already checked waiters (no match) but before
    /// `bus.wait()` registers a waiter — in that window the event would be
    /// lost to the waiter. By re-checking here we catch late-arriving waiters
    /// for Message events. Done events are handled by the track task's own
    /// waiter check + the `bus.wait()` post-registration agent-status check.
    fn push_event(&self, event: SubAgentEvent) {
        // Try to match a waiter for this event's target.
        let target = match &event {
            SubAgentEvent::Done { agent_id, .. } => agent_id.clone(),
            SubAgentEvent::Message { agent_id, .. } => agent_id.clone(),
        };
        let waiter_match = {
            let mut waiters = self.waiters.lock();
            let mut matched = None;
            // Match under the event source's own ID.
            if let Some(list) = waiters.get_mut(&target)
                && let Some(idx) = list
                    .iter()
                    .position(|w| w.from.is_none() || w.from.as_deref() == Some(&target))
            {
                let waiter = list.remove(idx);
                if list.is_empty() {
                    waiters.remove(&target);
                }
                matched = Some(waiter);
            }
            // Match under MAIN_AGENT_ID with from=Some(source).
            if matched.is_none()
                && let Some(list) = waiters.get_mut(MAIN_AGENT_ID)
                && let Some(idx) = list.iter().position(|w| w.from.as_deref() == Some(&target))
            {
                let waiter = list.remove(idx);
                if list.is_empty() {
                    waiters.remove(MAIN_AGENT_ID);
                }
                matched = Some(waiter);
            }
            matched
        };
        let event = match waiter_match {
            Some(waiter) => match waiter.tx.send(event) {
                Ok(()) => return,
                Err(err) => err, // tx closed; fall through to event sink
            },
            None => event,
        };
        let sink = self.event_sink.lock();
        if let Some(sink) = sink.as_ref() {
            sink(event);
        }
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
            // Deliver via oneshot — does not go through steer or event sink.
            // If tx.send() fails, the waiter's rx was dropped (timeout/abort in
            // progress). Fall through to steer/event-sink delivery below.
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

        // 3. Main agent: deliver via event sink.
        if is_main {
            self.push_event(SubAgentEvent::Message {
                agent_id: from.to_string(),
                body: body.to_string(),
            });
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

        // Race fix: the sub-agent may have completed between the track task's
        // waiter check and our registration above. If filter_from targets a
        // specific agent that is already Done, synthesize the event now.
        if let Some(from_id) = filter_from {
            let agents = self.agents.lock();
            if let Some(entry) = agents.get(from_id)
                && let AgentStatus::Done(result) = &entry.status
            {
                // Remove our waiter (we're returning early).
                let mut waiters = self.waiters.lock();
                if let Some(list) = waiters.get_mut(agent_id) {
                    list.retain(|w| !w.tx.is_closed());
                    if list.is_empty() {
                        waiters.remove(agent_id);
                    }
                }
                return Some(SubAgentEvent::Done {
                    agent_id: from_id.to_string(),
                    result: Ok((**result).clone()),
                });
            }
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

    /// Called by SpawnPlugin's AfterRunHook wait task when the idle timeout fires.
    pub(crate) fn set_idle_timeout_fired(&self) {
        self.idle_timeout_fired.store(true, Ordering::Relaxed);
    }

    /// Called by `should_stop`: consumes the flag (swap to false).
    /// Returns `true` if the idle timeout had fired since the last call.
    pub(crate) fn consume_idle_timeout_fired(&self) -> bool {
        self.idle_timeout_fired.swap(false, Ordering::Relaxed)
    }

    pub fn cancel_all(&self) {
        // Iterate and cancel — do NOT drain (mem::take). The OnAbortHook
        // contract requires re-callable: harness.abort() may be invoked again
        // after re-prompt. CancellationToken::cancel() is idempotent, so
        // cancelling already-cancelled tokens is harmless.
        let aborts = self.subagent_aborts.lock();
        for token in aborts.values() {
            token.cancel();
        }
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
    /// (via waiter if matched, else to event sink).
    #[cfg(test)]
    pub(crate) fn track(self: &Arc<Self>, handle: SubAgentHandle, agent_id: String) -> String {
        let abort = handle.abort_token();
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
            // case fall back to the event sink so the Done event is not lost.
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
            registry.push_event(event);

            registry.subagent_aborts.lock().remove(&id_for_remove);
            registry.remove_agent(&id_for_remove);
            in_flight.fetch_sub(1, Ordering::Relaxed);
        });

        agent_id
    }

    /// Track a sub-agent, returning an AsyncSpawnHandle.
    /// On completion: update status=Done, deliver SubAgentEvent::Done
    /// (via waiter if matched, else to event sink).
    pub(crate) fn track_with_oneshot(
        self: &Arc<Self>,
        handle: SubAgentHandle,
        agent_id: String,
    ) -> crate::delivery::AsyncSpawnHandle {
        let abort = handle.abort_token();
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

            // Try waiter match first, else event sink.
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
            // case fall back to the event sink so the Done event is not lost.
            let event = match waiter_match {
                Some(waiter) => match waiter.tx.send(event) {
                    Ok(()) => {
                        registry.subagent_aborts.lock().remove(&id_for_remove);
                        registry.remove_agent(&id_for_remove);
                        registry.in_flight.fetch_sub(1, Ordering::Relaxed);
                        return;
                    }
                    Err(e) => e, // rx dropped, fall back to event sink
                },
                None => event,
            };
            registry.push_event(event);

            registry.subagent_aborts.lock().remove(&id_for_remove);
            registry.remove_agent(&id_for_remove);
            registry.in_flight.fetch_sub(1, Ordering::Relaxed);
        });

        crate::delivery::AsyncSpawnHandle::new(agent_id, abort)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_agent::HarnessBuilder;
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
        let bus = MessageBus::new();

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
        let bus = MessageBus::new();
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
        let bus = MessageBus::new();
        let events = Arc::new(std::sync::Mutex::new(vec![]));
        let events_clone = events.clone();
        bus.set_event_sink(Arc::new(move |event| {
            events_clone.lock().unwrap().push(event);
        }));

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

        let evs = events.lock().unwrap();
        assert_eq!(evs.len(), 1, "one event should be pushed");
        match &evs[0] {
            SubAgentEvent::Message { agent_id, body } => {
                assert_eq!(agent_id, "sub-1");
                assert_eq!(body, "report from sub");
            }
            _ => panic!("expected Message event"),
        }
    }

    #[tokio::test]
    async fn send_to_unknown_agent_returns_failed() {
        let bus = MessageBus::new();

        let receipt = bus.send("main", "nonexistent", "hello").await;
        assert!(matches!(receipt, SendReceipt::Failed(_)));
    }

    #[tokio::test]
    async fn query_status_returns_all_agents() {
        let bus = MessageBus::new();

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
        let bus = MessageBus::new();

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
        let bus = MessageBus::new();

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
        let bus = MessageBus::new();

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
        let bus = MessageBus::new();

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
    async fn abort_agent_cancels_registered_token() {
        // Fix 4: abort_agent cancels a specific sub-agent's token.
        let bus = MessageBus::new();

        let token = CancellationToken::new();
        bus.register_subagent_abort("sub-1", token.clone());

        assert!(!token.is_cancelled());
        bus.abort_agent("sub-1");
        assert!(token.is_cancelled());

        // Aborting an unknown agent is a no-op (no panic).
        bus.abort_agent("nonexistent");
    }
}
