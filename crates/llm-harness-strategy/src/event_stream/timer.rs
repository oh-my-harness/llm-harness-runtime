//! Timer-driven event stream: one-shot and interval timers.
//!
//! Implements item 25 (Cron/Heartbeat 内置 EventStream 实现) — the timer
//! portion. Corresponds to the tutorial's `set_timer` tool (ch4 line 374).
//!
//! # Semantics
//!
//! - **One-shot** (`TimerKind::Once`): fires exactly one event at `fire_at`,
//!   then returns `None` on the next `next()` call (stream ended).
//!   `WaitForExternalEventTool` sees `event_stream_closed`, LLM decides
//!   next action.
//! - **Interval** (`TimerKind::Interval`): fires events at `first_fire_at`,
//!   then every `interval`. Never returns `None` — stopping is via `abort`
//!   (CancellationToken), which is handled by `WaitForExternalEventTool`'s
//!   `select!`.
//!
//! # Crash recovery
//!
//! `descriptor()` returns a serializable `Value` that can be passed to
//! `TaskStore::save_event_wait`. On restart, `from_descriptor()` rebuilds
//! the stream:
//! - `Once` + `fire_at` already passed → fires immediately
//! - `Once` + `fire_at` in future → sleeps until `fire_at`
//! - `Interval` + `first_fire_at` already passed → fires immediately,
//!   then resumes every `interval` (no schedule adjustment)
//! - `Interval` + `first_fire_at` in future → sleeps until `first_fire_at`

use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::template::EventDetails;
use llm_harness_types::{DataBlock, Event, EventStream};

/// Timer kind: one-shot or repeating interval.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimerKind {
    /// Fire once at `fire_at`, then stream ends (returns `None`).
    Once { fire_at: DateTime<Utc> },
    /// Fire at `first_fire_at`, then every `interval`.
    Interval {
        first_fire_at: DateTime<Utc>,
        interval: Duration,
    },
}

/// Timer-driven event stream.
pub struct TimerStream {
    kind: TimerKind,
    label: String,
    /// Tracks whether a one-shot timer has already fired.
    fired: bool,
}

impl TimerStream {
    /// Create a one-shot timer that fires at `fire_at`.
    /// After firing, `next()` returns `None`.
    pub fn once(fire_at: DateTime<Utc>, label: impl Into<String>) -> Self {
        Self {
            kind: TimerKind::Once { fire_at },
            label: label.into(),
            fired: false,
        }
    }

    /// Create an interval timer that fires at `first_fire_at`, then every
    /// `interval`. Never returns `None`.
    pub fn interval(
        first_fire_at: DateTime<Utc>,
        interval: Duration,
        label: impl Into<String>,
    ) -> Self {
        Self {
            kind: TimerKind::Interval {
                first_fire_at,
                interval,
            },
            label: label.into(),
            fired: false,
        }
    }

    /// Reconstruct a TimerStream from a serialized descriptor.
    /// Returns `None` if the descriptor is malformed.
    pub fn from_descriptor(descriptor: &Value) -> Option<Self> {
        let parsed: TimerDescriptor = serde_json::from_value(descriptor.clone()).ok()?;
        let label = parsed.label;
        let kind = match parsed.kind.as_str() {
            "once" => {
                let fire_at: DateTime<Utc> = serde_json::from_value(parsed.fire_at.clone()).ok()?;
                TimerKind::Once { fire_at }
            }
            "interval" => {
                let first_fire_at: DateTime<Utc> =
                    serde_json::from_value(parsed.first_fire_at.clone()).ok()?;
                let interval_ms = parsed.interval_ms?;
                TimerKind::Interval {
                    first_fire_at,
                    interval: Duration::from_millis(interval_ms),
                }
            }
            _ => return None,
        };
        Some(Self {
            kind,
            label,
            fired: false,
        })
    }

    /// Serialize a descriptor for `TaskStore::save_event_wait`.
    pub fn descriptor(&self) -> Value {
        match &self.kind {
            TimerKind::Once { fire_at } => serde_json::json!({
                "type": "timer",
                "kind": "once",
                "fire_at": fire_at,
                "label": self.label,
            }),
            TimerKind::Interval {
                first_fire_at,
                interval,
            } => serde_json::json!({
                "type": "timer",
                "kind": "interval",
                "first_fire_at": first_fire_at,
                "interval_ms": interval.as_millis() as u64,
                "label": self.label,
            }),
        }
    }

    /// Build the Event for a timer fire.
    fn make_event(&self) -> Event {
        let (channel, reason) = match &self.kind {
            TimerKind::Once { fire_at } => ("timer.once", format!("scheduled fire at {}", fire_at)),
            TimerKind::Interval { .. } => ("timer.interval", "interval fire".to_string()),
        };
        let details = EventDetails::new("timer", channel)
            .with_label(&self.label)
            .with_reason(reason);
        Event {
            content: vec![DataBlock::text(format!("timer fired: {}", self.label))],
            details: details.to_value(),
        }
    }

    /// Compute the sleep duration until the next fire.
    fn sleep_until_next(&self) -> Duration {
        let now = Utc::now();
        match &self.kind {
            TimerKind::Once { fire_at } => {
                if *fire_at <= now {
                    Duration::ZERO
                } else {
                    (*fire_at - now).to_std().unwrap_or(Duration::ZERO)
                }
            }
            TimerKind::Interval {
                first_fire_at,
                interval,
            } => {
                if self.fired {
                    // Already fired at least once: sleep for full interval.
                    *interval
                } else if *first_fire_at <= now {
                    Duration::ZERO
                } else {
                    (*first_fire_at - now).to_std().unwrap_or(Duration::ZERO)
                }
            }
        }
    }
}

impl EventStream for TimerStream {
    fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<Event>> {
        Box::pin(async move {
            // One-shot already fired → stream ended.
            if matches!(self.kind, TimerKind::Once { .. }) && self.fired {
                return None;
            }

            let sleep_dur = self.sleep_until_next();
            if !sleep_dur.is_zero() {
                tokio::time::sleep(sleep_dur).await;
            }

            self.fired = true;
            Some(self.make_event())
        })
    }
}

/// Internal struct for deserializing descriptors.
#[derive(Deserialize)]
struct TimerDescriptor {
    label: String,
    kind: String,
    #[serde(default)]
    fire_at: Value,
    #[serde(default)]
    first_fire_at: Value,
    interval_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn once_timer_fires_then_returns_none() {
        let fire_at = Utc::now() + chrono::Duration::milliseconds(100);
        let mut stream = TimerStream::once(fire_at, "test-once");

        // First next() → fires after ~100ms
        let e1 = stream.next().await.unwrap();
        assert_eq!(e1.details["source"], "timer");
        assert_eq!(e1.details["channel"], "timer.once");
        assert_eq!(e1.details["context"]["label"], "test-once");

        // Second next() → None (stream ended)
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn once_timer_with_past_fire_at_fires_immediately() {
        let fire_at = Utc::now() - chrono::Duration::seconds(5);
        let mut stream = TimerStream::once(fire_at, "past");

        let e = stream.next().await.unwrap();
        assert_eq!(e.details["channel"], "timer.once");
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn interval_timer_fires_multiple_times() {
        let first = Utc::now() + chrono::Duration::milliseconds(50);
        let interval = Duration::from_millis(50);
        let mut stream = TimerStream::interval(first, interval, "poll");

        let e1 = stream.next().await.unwrap();
        assert_eq!(e1.details["channel"], "timer.interval");

        let e2 = stream.next().await.unwrap();
        assert_eq!(e2.details["channel"], "timer.interval");

        let e3 = stream.next().await.unwrap();
        assert_eq!(e3.details["channel"], "timer.interval");

        // Interval never returns None — we just stop calling.
    }

    #[tokio::test]
    async fn once_timer_descriptor_roundtrip() {
        let fire_at = Utc::now() + chrono::Duration::seconds(60);
        let original = TimerStream::once(fire_at, "roundtrip");
        let desc = original.descriptor();

        assert_eq!(desc["type"], "timer");
        assert_eq!(desc["kind"], "once");
        assert_eq!(desc["label"], "roundtrip");

        let rebuilt = TimerStream::from_descriptor(&desc).unwrap();
        assert_eq!(rebuilt.label, "roundtrip");
        match &rebuilt.kind {
            TimerKind::Once { fire_at: fa } => {
                assert_eq!(fa, &fire_at);
            }
            _ => panic!("expected Once"),
        }
    }

    #[tokio::test]
    async fn interval_timer_descriptor_roundtrip() {
        let first = Utc::now() + chrono::Duration::seconds(30);
        let interval = Duration::from_secs(60);
        let original = TimerStream::interval(first, interval, "poll-ci");
        let desc = original.descriptor();

        assert_eq!(desc["type"], "timer");
        assert_eq!(desc["kind"], "interval");
        assert_eq!(desc["interval_ms"], 60_000);

        let rebuilt = TimerStream::from_descriptor(&desc).unwrap();
        assert_eq!(rebuilt.label, "poll-ci");
        match &rebuilt.kind {
            TimerKind::Interval {
                first_fire_at,
                interval: iv,
            } => {
                assert_eq!(first_fire_at, &first);
                assert_eq!(*iv, interval);
            }
            _ => panic!("expected Interval"),
        }
    }

    #[tokio::test]
    async fn from_descriptor_rejects_malformed() {
        assert!(TimerStream::from_descriptor(&serde_json::json!({"foo": 1})).is_none());
        assert!(TimerStream::from_descriptor(&serde_json::json!("not object")).is_none());
        assert!(
            TimerStream::from_descriptor(&serde_json::json!({
                "type": "timer",
                "kind": "unknown",
                "label": "x",
                "fire_at": "2026-01-01T00:00:00Z",
                "first_fire_at": "2026-01-01T00:00:00Z",
            }))
            .is_none()
        );
    }

    #[tokio::test]
    async fn from_descriptor_rebuilds_past_once_as_immediate() {
        // fire_at in the past → rebuilt stream should fire immediately
        let fire_at = Utc::now() - chrono::Duration::seconds(10);
        let desc = serde_json::json!({
            "type": "timer",
            "kind": "once",
            "fire_at": fire_at,
            "label": "past-rebuild",
            "first_fire_at": fire_at,
        });
        let mut rebuilt = TimerStream::from_descriptor(&desc).unwrap();

        let e = rebuilt.next().await.unwrap();
        assert_eq!(e.details["context"]["label"], "past-rebuild");
        assert!(rebuilt.next().await.is_none());
    }
}
