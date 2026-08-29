//! SpawnPlugin: replaces AsyncSpawnHook + IdleWatcher + AbortCascadeHook.
//!
//! Holds its own completion queue (VecDeque + Notify), eliminating the shared
//! event channel. Routes sub-agent events through 4 generic hooks.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::delivery::{format_failure, format_success};
use crate::message_bus::{MessageBus, SubAgentEvent};
use llm_harness_agent::{AgentHarness, HarnessHooks, Plugin};
use llm_harness_types::{
    AfterRunHook, AfterTurnCtx, AfterTurnHook, AgentError, AgentMessage, BeforeRunCtx,
    BeforeRunHook, BeforeRunResult, CustomMessage, OnAbortHook,
};

pub struct SpawnPlugin {
    bus: Arc<MessageBus>,
    queue: Arc<Mutex<VecDeque<SubAgentEvent>>>,
    notify: Arc<Notify>,
    harness_weak: Arc<Mutex<Option<Weak<AgentHarness>>>>,
    wait_task_token: Arc<Mutex<Option<CancellationToken>>>,
    aborted: Arc<std::sync::atomic::AtomicBool>,
    idle_timeout: Duration,
}

impl SpawnPlugin {
    pub fn new(bus: Arc<MessageBus>) -> Arc<Self> {
        Self::new_with_idle_timeout(bus, Duration::from_secs(60))
    }

    pub fn new_with_idle_timeout(bus: Arc<MessageBus>, idle_timeout: Duration) -> Arc<Self> {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let notify = Arc::new(Notify::new());
        let queue_clone = queue.clone();
        let notify_clone = notify.clone();
        bus.set_event_sink(Arc::new(move |event| {
            // Ok Done payloads live in the bus's completed buffer (single
            // source of truth); the sink is only a wakeup signal for them.
            // Failed Done events (Err, non-Aborted) are never buffered, so
            // they carry their payload through the queue for injection.
            // Message events also carry their own payload — enqueue as before.
            let is_wakeup_only = matches!(event, SubAgentEvent::Done { result: Ok(_), .. });
            if !is_wakeup_only {
                queue_clone.lock().unwrap().push_back(event);
            }
            notify_clone.notify_one();
        }));
        Arc::new(Self {
            bus,
            queue,
            notify,
            harness_weak: Arc::new(Mutex::new(None)),
            wait_task_token: Arc::new(Mutex::new(None)),
            aborted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            idle_timeout,
        })
    }

    /// Test-only view of the plugin's pending event queue.
    #[cfg(test)]
    pub(crate) fn queue_for_test(&self) -> Arc<Mutex<VecDeque<SubAgentEvent>>> {
        self.queue.clone()
    }

    pub fn set_harness_weak(&self, weak: Weak<AgentHarness>) {
        *self.harness_weak.lock().unwrap() = Some(weak);
    }
}

impl Plugin for SpawnPlugin {
    fn name(&self) -> &str {
        "spawn"
    }

    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        hooks.before_run.push(Arc::new(SpawnBeforeRun {
            queue: self.queue.clone(),
            bus: self.bus.clone(),
            wait_task_token: self.wait_task_token.clone(),
            aborted: self.aborted.clone(),
        }));
        hooks.after_turn.push(Arc::new(SpawnAfterTurn {
            queue: self.queue.clone(),
            bus: self.bus.clone(),
            harness_weak: self.harness_weak.clone(),
        }));
        hooks.after_run.push(Arc::new(SpawnAfterRun {
            harness_weak: self.harness_weak.clone(),
            queue: self.queue.clone(),
            notify: self.notify.clone(),
            wait_task_token: self.wait_task_token.clone(),
            aborted: self.aborted.clone(),
            bus: self.bus.clone(),
            idle_timeout: self.idle_timeout,
        }));
        hooks.on_abort.push(Arc::new(SpawnOnAbort {
            bus: self.bus.clone(),
            aborted: self.aborted.clone(),
            wait_task_token: self.wait_task_token.clone(),
        }));
    }
}

// ── SpawnBeforeRun ───────────────────────────────────────────────────────────

struct SpawnBeforeRun {
    queue: Arc<Mutex<VecDeque<SubAgentEvent>>>,
    bus: Arc<MessageBus>,
    wait_task_token: Arc<Mutex<Option<CancellationToken>>>,
    aborted: Arc<std::sync::atomic::AtomicBool>,
}

impl BeforeRunHook for SpawnBeforeRun {
    fn before_run<'a>(
        &'a self,
        _ctx: BeforeRunCtx<'a>,
    ) -> BoxFuture<'a, Result<BeforeRunResult, AgentError>> {
        Box::pin(async move {
            // Clear stale aborted flag (from Idle-state abort that left it set).
            self.aborted
                .store(false, std::sync::atomic::Ordering::Relaxed);
            // Cancel pending wait task (if any).
            if let Some(token) = self.wait_task_token.lock().unwrap().take() {
                token.cancel();
            }
            let mut events: Vec<SubAgentEvent> = {
                let mut q = self.queue.lock().unwrap();
                q.drain(..).collect()
            };
            // Merge Done results from the bus's completed buffer (they are
            // not enqueued into the queue anymore — sink is wakeup-only
            // for Done).
            for (agent_id, result) in self.bus.drain_completed() {
                events.push(SubAgentEvent::Done {
                    agent_id,
                    result: Ok(result),
                });
            }
            let additional = format_events(events);
            Ok(BeforeRunResult {
                additional_messages: additional,
                system_prompt: None,
            })
        })
    }
}

// ── SpawnAfterTurn ───────────────────────────────────────────────────────────

struct SpawnAfterTurn {
    queue: Arc<Mutex<VecDeque<SubAgentEvent>>>,
    bus: Arc<MessageBus>,
    harness_weak: Arc<Mutex<Option<Weak<AgentHarness>>>>,
}

impl AfterTurnHook for SpawnAfterTurn {
    fn after_turn<'a>(&'a self, _ctx: AfterTurnCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let mut events: Vec<SubAgentEvent> = {
                let mut q = self.queue.lock().unwrap();
                q.drain(..).collect()
            };
            for (agent_id, result) in self.bus.drain_completed() {
                events.push(SubAgentEvent::Done {
                    agent_id,
                    result: Ok(result),
                });
            }
            let msg = (!events.is_empty())
                .then(|| format_events(events))
                .and_then(|mut m| (!m.is_empty()).then(|| m.remove(0)))
                .or_else(|| status_hint(&self.bus));
            // Clone Weak out of the mutex, then drop the guard before
            // upgrading + sending. `follow_up_message` locks HarnessInner,
            // so holding the harness_weak guard across it would nest locks.
            let weak = {
                let guard = self.harness_weak.lock().unwrap();
                guard.as_ref().map(|w| w.clone())
            };
            if let Some(msg) = msg
                && let Some(h) = weak.as_ref().and_then(|w| w.upgrade())
                && let Err(e) = h.follow_up_message(msg)
            {
                tracing::warn!("follow_up message dropped: {:?}", e);
            }
        })
    }
}

// ── SpawnAfterRun ────────────────────────────────────────────────────────────

struct SpawnAfterRun {
    harness_weak: Arc<Mutex<Option<Weak<AgentHarness>>>>,
    queue: Arc<Mutex<VecDeque<SubAgentEvent>>>,
    notify: Arc<Notify>,
    wait_task_token: Arc<Mutex<Option<CancellationToken>>>,
    aborted: Arc<std::sync::atomic::AtomicBool>,
    bus: Arc<MessageBus>,
    idle_timeout: Duration,
}

impl AfterRunHook for SpawnAfterRun {
    fn after_run(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if self
                .aborted
                .swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                return;
            }
            {
                let has_work =
                    !self.queue.lock().unwrap().is_empty() || self.bus.has_unclaimed_done();
                if has_work {
                    self.wake_harness();
                    return;
                }
            }
            if self.bus.in_flight_count() == 0 {
                // Double-check queue: track task may have pushed an event
                // between the queue check above and this in_flight check.
                if !self.queue.lock().unwrap().is_empty() || self.bus.has_unclaimed_done() {
                    self.wake_harness();
                }
                return;
            }
            // Cancel any previous wait task before spawning a new one,
            // preventing orphaned tasks that compete for the same Notify.
            if let Some(old) = self.wait_task_token.lock().unwrap().take() {
                old.cancel();
            }
            let token = CancellationToken::new();
            *self.wait_task_token.lock().unwrap() = Some(token.clone());
            let harness_weak = self.harness_weak.clone();
            let notify = self.notify.clone();
            let bus = self.bus.clone();
            let queue = self.queue.clone();
            let timeout = self.idle_timeout;
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = notify.notified() => {
                            // Sub-agent completed or sent a message. Check the
                            // queue: if non-empty, continue_run will drain it
                            // via before_run hook and inject into LLM context.
                            // If empty and in_flight > 0, the notification was
                            // spurious — wait again.
                            let has_events = !queue.lock().unwrap().is_empty()
                                || bus.has_unclaimed_done();
                            if has_events {
                                let weak = harness_weak.lock().unwrap().as_ref().and_then(|w| w.upgrade());
                                if let Some(h) = weak {
                                    let _ = h.continue_run().await;
                                }
                            }
                            if bus.in_flight_count() == 0 {
                                break;
                            }
                        }
                        _ = tokio::time::sleep(timeout) => {
                            if bus.in_flight_count() == 0 {
                                break;
                            }
                            bus.set_idle_timeout_fired();
                            let weak = harness_weak.lock().unwrap().as_ref().and_then(|w| w.upgrade());
                            if let Some(h) = weak {
                                let _ = h.continue_run().await;
                            }
                            if bus.in_flight_count() == 0 {
                                break;
                            }
                        }
                        _ = token.cancelled() => {
                            // Cancelled by SpawnBeforeRun (via continue_run →
                            // before_run hook) or by a new after_run spawning
                            // a replacement wait task. If continue_run failed
                            // (NotIdle race), the stale token is cleaned up
                            // by the next after_run's take()+cancel() above.
                            break;
                        }
                    }
                }
            });
        })
    }
}

impl SpawnAfterRun {
    fn wake_harness(&self) {
        if let Some(weak) = self.harness_weak.lock().unwrap().as_ref()
            && let Some(h) = weak.upgrade()
        {
            let h = h.clone();
            tokio::spawn(async move {
                let _ = h.continue_run().await;
            });
        }
    }
}

// ── SpawnOnAbort ─────────────────────────────────────────────────────────────

struct SpawnOnAbort {
    bus: Arc<MessageBus>,
    aborted: Arc<std::sync::atomic::AtomicBool>,
    wait_task_token: Arc<Mutex<Option<CancellationToken>>>,
}

impl OnAbortHook for SpawnOnAbort {
    fn on_abort(&self) {
        self.aborted
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(token) = self.wait_task_token.lock().unwrap().take() {
            token.cancel();
        }
        self.bus.cancel_all();
    }
}

// ── Formatting helpers ───────────────────────────────────────────────────────

fn format_events(events: Vec<SubAgentEvent>) -> Vec<AgentMessage> {
    if events.is_empty() {
        return vec![];
    }
    let mut results: Vec<String> = Vec::new();
    for event in events {
        match event {
            SubAgentEvent::Done { agent_id, result } => match result {
                Ok(r) => results.push(format_success(&agent_id, &r)),
                Err(AgentError::Aborted) => {}
                Err(e) => results.push(format_failure(&agent_id, &e)),
            },
            SubAgentEvent::Message { agent_id, body } => {
                results.push(format!("[from:{agent_id}] {body}"));
            }
        }
    }
    if results.is_empty() {
        return vec![];
    }
    let packed = results.join("\n\n---\n\n");
    vec![AgentMessage::Custom(CustomMessage {
        r#type: "sub_agent_message".into(),
        data: serde_json::json!({"messages": packed}),
        timestamp: chrono::Utc::now(),
    })]
}

fn status_hint(bus: &Arc<MessageBus>) -> Option<AgentMessage> {
    if !bus.consume_idle_timeout_fired() {
        return None;
    }
    let in_flight = bus.in_flight_count();
    if in_flight == 0 {
        return None;
    }
    let snapshots = bus.query_status(None);
    let running: Vec<&str> = snapshots
        .iter()
        .filter(|s| matches!(s.status, crate::message_bus::AgentStatus::Running))
        .map(|s| s.agent_id.as_str())
        .collect();
    if running.is_empty() {
        return None;
    }
    let hint = format!(
        "{n} sub-agent(s) still running: {ids}. \
         Use query_subagent to check status or await_subagent_reply to wait for results.",
        n = running.len(),
        ids = running.join(", ")
    );
    Some(AgentMessage::Custom(CustomMessage {
        r#type: "sub_agent_message".into(),
        data: serde_json::json!({"messages": hint}),
        timestamp: chrono::Utc::now(),
    }))
}
