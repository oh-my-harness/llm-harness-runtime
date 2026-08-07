//! Heartbeat (activity-driven watchdog) event stream.
//!
//! Implements item 25 (Cron/Heartbeat 内置 EventStream 实现) — the heartbeat
//! portion. Corresponds to the tutorial's Heartbeat mechanism (ch4,
//! OpenClaw three mechanisms: Hooks / Cron / Heartbeat).
//!
//!
//! Unlike `TimerStream` (time-driven: "fire every N seconds"),
//! `HeartbeatStream` is activity-driven: "if no activity for N seconds,
//! emit a heartbeat warning". The business layer calls `HeartbeatHandle::tick()`
//! on every meaningful activity (tool call, LLM response, user interaction)
//! to reset the watchdog.
//!
//! # Architecture
//!
//! Split into two objects sharing an `Arc<AtomicU64>` (last-activity
//! timestamp) and an `Arc<tokio::sync::Notify>` (wake-up signal):
//! - `HeartbeatHandle` — lightweight `Clone + Send + Sync` control handle
//!   with `tick()` method. The business layer holds this.
//! - `HeartbeatStream` — implements `EventStream`, held by
//!   `WaitForExternalEventTool` as `Box<dyn EventStream>`.
//!
//! This separation is necessary because `WaitForExternalEventTool` holds
//! `Arc<Mutex<Box<dyn EventStream>>>`, which type-erases the concrete type.
//! The handle provides out-of-band control without needing access to the
//! trait object.
//!
//! `tick()` updates the timestamp and notifies the stream via `Notify`,
//! interrupting any in-progress sleep so the watchdog immediately
//! recomputes the remaining wait.
//!
//! # Crash recovery
//!
//! `descriptor()` returns a serializable `Value`. On restart,
//! `from_descriptor()` rebuilds the stream. Since `last_activity` is lost
//! on crash, if `saved_at` (from `EventWaitRecord`) is older than
//! `timeout` relative to `now`, the heartbeat fires immediately.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;

use super::template::EventDetails;
use llm_harness_types::{DataBlock, Event, EventStream};

/// Current time as epoch millis.
fn now_millis() -> u64 {
    Utc::now().timestamp_millis() as u64
}

/// Lightweight control handle for resetting the heartbeat watchdog.
///
/// `Clone + Send + Sync` — safe to share across threads. The business
/// layer holds this and calls `tick()` on every meaningful activity.
#[derive(Clone)]
pub struct HeartbeatHandle {
    last_activity: Arc<AtomicU64>,
    notify: Arc<tokio::sync::Notify>,
}

impl HeartbeatHandle {
    /// Reset the watchdog — call on every meaningful activity.
    ///
    /// Updates the activity timestamp and wakes the stream's sleep so it
    /// immediately recomputes the remaining wait.
    pub fn tick(&self) {
        self.last_activity.store(now_millis(), Ordering::Relaxed);
        self.notify.notify_one();
    }
}

/// Activity-driven watchdog event stream.
///
/// Emits a heartbeat event if no `tick()` is received within `timeout`.
/// Never returns `None` — stopping is via `abort` (CancellationToken).
pub struct HeartbeatStream {
    last_activity: Arc<AtomicU64>,
    notify: Arc<tokio::sync::Notify>,
    timeout: Duration,
    label: String,
}

impl HeartbeatStream {
    /// Create a new heartbeat stream. Returns a handle (for `tick()`) and
    /// a boxed stream (for `WaitForExternalEventTool`).
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        timeout: Duration,
        label: impl Into<String>,
    ) -> (HeartbeatHandle, Box<dyn EventStream>) {
        let last_activity = Arc::new(AtomicU64::new(now_millis()));
        let notify = Arc::new(tokio::sync::Notify::new());
        let handle = HeartbeatHandle {
            last_activity: last_activity.clone(),
            notify: notify.clone(),
        };
        let stream = HeartbeatStream {
            last_activity,
            notify,
            timeout,
            label: label.into(),
        };
        (handle, Box::new(stream))
    }

    /// Reconstruct from a serialized descriptor.
    ///
    /// `saved_at` is the timestamp from `EventWaitRecord.saved_at` (written by
    /// the runtime when `save_event_wait` persisted the descriptor). If the
    /// process was down longer than `timeout`, the heartbeat fires immediately.
    pub fn from_descriptor(
        descriptor: &Value,
        saved_at: Option<DateTime<Utc>>,
    ) -> Option<(HeartbeatHandle, Box<dyn EventStream>)> {
        let parsed: HeartbeatDescriptor = serde_json::from_value(descriptor.clone()).ok()?;
        let timeout = Duration::from_millis(parsed.timeout_ms);
        let baseline = saved_at
            .map(|dt| dt.timestamp_millis() as u64)
            .unwrap_or_else(now_millis);
        let last_activity = Arc::new(AtomicU64::new(baseline));
        let notify = Arc::new(tokio::sync::Notify::new());
        let handle = HeartbeatHandle {
            last_activity: last_activity.clone(),
            notify: notify.clone(),
        };
        let stream = HeartbeatStream {
            last_activity,
            notify,
            timeout,
            label: parsed.label,
        };
        Some((handle, Box::new(stream)))
    }

    /// Serialize a descriptor for `TaskStore::save_event_wait`.
    /// The `saved_at` timestamp is set by the runtime when `save_event_wait`
    /// persists the `EventWaitRecord` — it is NOT embedded here. On recovery,
    /// pass `EventWaitRecord.saved_at` to `from_descriptor` separately.
    pub fn descriptor(&self) -> Value {
        serde_json::json!({
            "type": "heartbeat",
            "timeout_ms": self.timeout.as_millis() as u64,
            "label": self.label,
        })
    }

    fn make_event(&self) -> Event {
        let details = EventDetails::new("heartbeat", "heartbeat.timeout")
            .with_label(&self.label)
            .with_reason(format!(
                "no activity for {:?} seconds",
                self.timeout.as_secs()
            ));
        Event {
            content: vec![DataBlock::text(format!(
                "heartbeat timeout: {}",
                self.label
            ))],
            details: details.to_value(),
        }
    }
}

impl EventStream for HeartbeatStream {
    fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<Event>> {
        Box::pin(async move {
            loop {
                let last = self.last_activity.load(Ordering::Relaxed) as i64;
                let now = now_millis() as i64;
                let elapsed_ms = (now - last).max(0) as u64;
                let timeout_ms = self.timeout.as_millis() as u64;
                let remaining_ms = timeout_ms.saturating_sub(elapsed_ms);

                if remaining_ms == 0 {
                    // Timeout: emit heartbeat and reset.
                    self.last_activity.store(now_millis(), Ordering::Relaxed);
                    return Some(self.make_event());
                }

                // Sleep for remaining time, but wake early if tick() arrives.
                let sleep_dur = Duration::from_millis(remaining_ms);
                tokio::select! {
                    _ = tokio::time::sleep(sleep_dur) => {}
                    _ = self.notify.notified() => {}
                }
            }
        })
    }
}

/// Internal struct for deserializing descriptors.
#[derive(Deserialize)]
struct HeartbeatDescriptor {
    timeout_ms: u64,
    label: String,
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn heartbeat_fires_after_timeout() {
        let (_handle, mut stream) = HeartbeatStream::new(Duration::from_millis(100), "watchdog");

        // No tick() → should fire after ~100ms
        let e = stream.next().await.unwrap();
        assert_eq!(e.details["source"], "heartbeat");
        assert_eq!(e.details["channel"], "heartbeat.timeout");
        assert_eq!(e.details["context"]["label"], "watchdog");
    }

    #[tokio::test]
    async fn heartbeat_reset_by_tick_delays_fire() {
        let (handle, mut stream) = HeartbeatStream::new(Duration::from_millis(200), "watchdog");

        // Tick at 50ms → timeout pushed to ~250ms
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.tick();

        // Without tick, fire would be at ~200ms. With tick at 50ms,
        // fire should be at ~250ms. Verify it does NOT fire before 200ms
        // (the original deadline) by checking that a 150ms timeout on
        // stream.next() elapses without a result.
        let early = tokio::time::timeout(Duration::from_millis(150), stream.next()).await;
        assert!(early.is_err(), "should not fire before original deadline");

        // Now it should fire within the remaining ~100ms
        let e = tokio::time::timeout(Duration::from_millis(200), stream.next())
            .await
            .expect("should fire within extended deadline")
            .unwrap();
        assert_eq!(e.details["channel"], "heartbeat.timeout");
    }

    #[tokio::test]
    async fn tick_interrupts_sleep_and_delays_fire() {
        // timeout = 300ms. Without tick, fires at ~300ms.
        // tick() at ~50ms should interrupt the sleep and push fire to ~350ms.
        let (handle, mut stream) = HeartbeatStream::new(Duration::from_millis(300), "watchdog");

        // Start next() in a concurrent task so we can call tick() while it sleeps.
        let task = tokio::spawn(async move { stream.next().await });

        // Tick at 50ms — interrupts the 300ms sleep, pushes deadline to ~350ms.
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.tick();

        // At 200ms (well before the 350ms deadline) the task should still be pending.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !task.is_finished(),
            "should not fire before tick-extended deadline"
        );

        // Wait for the fire (should happen around 350ms).
        let e = tokio::time::timeout(Duration::from_millis(200), task)
            .await
            .expect("should fire within extended deadline")
            .unwrap()
            .unwrap();
        assert_eq!(e.details["channel"], "heartbeat.timeout");
    }

    #[tokio::test]
    async fn heartbeat_fires_repeatedly() {
        let (_handle, mut stream) = HeartbeatStream::new(Duration::from_millis(50), "loop");

        let e1 = stream.next().await.unwrap();
        assert_eq!(e1.details["channel"], "heartbeat.timeout");

        let e2 = stream.next().await.unwrap();
        assert_eq!(e2.details["channel"], "heartbeat.timeout");
    }

    #[tokio::test]
    async fn heartbeat_handle_is_clone_and_send() {
        let (handle, _stream) = HeartbeatStream::new(Duration::from_secs(60), "test");

        // Clone and send to another thread
        let h2 = handle.clone();
        let h3 = handle.clone();
        let join = tokio::spawn(async move {
            h2.tick();
            h3.tick();
        });
        join.await.unwrap();
        handle.tick();
    }

    #[tokio::test]
    async fn heartbeat_descriptor_roundtrip() {
        // Construct the concrete struct directly (not via `new`, which
        // returns `Box<dyn EventStream>`) so we can call `descriptor()`.
        let stream = HeartbeatStream {
            last_activity: Arc::new(AtomicU64::new(now_millis())),
            notify: Arc::new(tokio::sync::Notify::new()),
            timeout: Duration::from_secs(30),
            label: "svc-health".to_string(),
        };
        let saved_at = Utc::now();
        let desc = stream.descriptor();

        assert_eq!(desc["type"], "heartbeat");
        assert_eq!(desc["timeout_ms"], 30_000);
        assert_eq!(desc["label"], "svc-health");

        let (rebuilt_handle, _rebuilt_stream) =
            HeartbeatStream::from_descriptor(&desc, Some(saved_at)).unwrap();
        // Rebuilt handle should work
        rebuilt_handle.tick();
    }
    #[tokio::test]
    async fn heartbeat_from_descriptor_with_old_saved_at_fires_immediately() {
        // saved_at 1 hour ago, timeout 10s → should fire immediately
        let old = Utc::now() - chrono::Duration::hours(1);
        let desc = serde_json::json!({
            "type": "heartbeat",
            "timeout_ms": 10_000,
            "label": "stale",
        });

        let (_handle, mut stream) = HeartbeatStream::from_descriptor(&desc, Some(old)).unwrap();
        let start = std::time::Instant::now();
        let e = stream.next().await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "should fire immediately, took {:?}",
            elapsed
        );
        assert_eq!(e.details["context"]["label"], "stale");
    }

    #[tokio::test]
    async fn heartbeat_from_descriptor_rejects_malformed() {
        assert!(HeartbeatStream::from_descriptor(&serde_json::json!({"foo": 1}), None).is_none());
        assert!(HeartbeatStream::from_descriptor(&serde_json::json!("not object"), None).is_none());
    }
}
