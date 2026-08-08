//! Event filtering wrapper for `EventStream`.
//!
//! Wraps an inner `Box<dyn EventStream>` and silently drops events that do
//! not match a predicate. Non-matching events never reach
//! `WaitForExternalEventTool`, avoiding unnecessary agent wake-ups.
//!
//! Implements item 42 (事件过滤机制):
//!
//! > "事件过滤机制（基于 Event.details 的 predicate 过滤）"
//!
//! # Design decisions
//!
//! - **Silent drop, no annotation**: Filtered events are silently discarded.
//!   No `filtered_count` is attached to matching events. The filter's job is
//!   to filter, not to count.
//! - **Stateless predicates**: `Fn(&Event) -> bool + Send`. Stateful filtering
//!   (e.g. "first 3 pass, then filter") is left to custom EventStream impls.
//! - **Inner None propagates**: When the inner stream returns `None`, the
//!   filter stream also returns `None` (stream ended).

use futures::future::BoxFuture;
use serde_json::Value;

use super::template::EventDetails;
use llm_harness_types::{Event, EventStream};

/// Event filter wrapper. Implements `EventStream` by delegating to an inner
/// stream and dropping events that fail the predicate.
pub struct FilterStream {
    inner: Box<dyn EventStream>,
    predicate: Box<dyn Fn(&Event) -> bool + Send>,
}

impl FilterStream {
    /// Create a new filter wrapping `inner`, dropping events where
    /// `predicate` returns `false`.
    pub fn new(inner: Box<dyn EventStream>, predicate: Box<dyn Fn(&Event) -> bool + Send>) -> Self {
        Self { inner, predicate }
    }
}

impl EventStream for FilterStream {
    fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<Event>> {
        Box::pin(async move {
            loop {
                match self.inner.next().await {
                    None => return None,
                    Some(ev) => {
                        if (self.predicate)(&ev) {
                            return Some(ev);
                        }
                        // Non-matching: silently drop, continue loop.
                    }
                }
            }
        })
    }
}

// ── Predicate factories ─────────────────────────────────────────────────

/// Filter events by `EventDetails.source` field.
///
/// Requires `Event.details` to match the `EventDetails` schema. Events
/// with non-conforming `details` (e.g. arbitrary `WebhookStream` payloads)
/// are silently dropped.
pub fn by_source(source: &str) -> Box<dyn Fn(&Event) -> bool + Send> {
    let source = source.to_string();
    Box::new(move |ev: &Event| {
        EventDetails::from_value(&ev.details)
            .map(|d| d.source == source)
            .unwrap_or(false)
    })
}
/// Filter events by `EventDetails.channel` field.
///
/// Requires `Event.details` to match the `EventDetails` schema. Events
/// with non-conforming `details` (e.g. arbitrary `WebhookStream` payloads)
/// are silently dropped.
pub fn by_channel(channel: &str) -> Box<dyn Fn(&Event) -> bool + Send> {
    let channel = channel.to_string();
    Box::new(move |ev: &Event| {
        EventDetails::from_value(&ev.details)
            .map(|d| d.channel == channel)
            .unwrap_or(false)
    })
}

/// Filter events by an arbitrary field in `Event.details`.
/// Matches if `details[field] == value`.
pub fn by_field(field: &str, value: &Value) -> Box<dyn Fn(&Event) -> bool + Send> {
    let field = field.to_string();
    let value = value.clone();
    Box::new(move |ev: &Event| ev.details.get(&field) == Some(&value))
}

/// Pass-through predicate that matches all events.
pub fn pass_all() -> Box<dyn Fn(&Event) -> bool + Send> {
    Box::new(|_: &Event| true)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::DataBlock;
    use tokio::sync::mpsc;

    /// Channel-backed stream for testing, mirroring the pattern in event.rs tests.
    struct ChannelStream {
        rx: mpsc::Receiver<Event>,
    }

    impl EventStream for ChannelStream {
        fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<Event>> {
            Box::pin(async move { self.rx.recv().await })
        }
    }

    fn make_event(source: &str, channel: &str) -> Event {
        let details = EventDetails::new(source, channel).to_value();
        Event {
            content: vec![DataBlock::text(format!("{source}/{channel}"))],
            details,
        }
    }

    fn make_raw_event(details: Value) -> Event {
        Event {
            content: vec![DataBlock::text("raw")],
            details,
        }
    }

    #[tokio::test]
    async fn filter_passes_matching_events() {
        let (tx, rx) = mpsc::channel(4);
        let inner: Box<dyn EventStream> = Box::new(ChannelStream { rx });
        let mut filter = FilterStream::new(inner, by_source("timer"));

        tx.send(make_event("timer", "timer.once")).await.unwrap();
        tx.send(make_event("heartbeat", "heartbeat.timeout"))
            .await
            .unwrap();
        tx.send(make_event("timer", "timer.interval"))
            .await
            .unwrap();
        drop(tx);

        // First event: timer → passes
        let e1 = filter.next().await.unwrap();
        assert_eq!(e1.details["source"], "timer");
        // Second event: heartbeat → dropped, third: timer → passes
        let e2 = filter.next().await.unwrap();
        assert_eq!(e2.details["channel"], "timer.interval");
        // Stream ended
        assert!(filter.next().await.is_none());
    }

    #[tokio::test]
    async fn filter_by_channel() {
        let (tx, rx) = mpsc::channel(4);
        let inner: Box<dyn EventStream> = Box::new(ChannelStream { rx });
        let mut filter = FilterStream::new(inner, by_channel("heartbeat.timeout"));

        tx.send(make_event("timer", "timer.once")).await.unwrap();
        tx.send(make_event("heartbeat", "heartbeat.timeout"))
            .await
            .unwrap();
        drop(tx);

        let e = filter.next().await.unwrap();
        assert_eq!(e.details["channel"], "heartbeat.timeout");
        assert!(filter.next().await.is_none());
    }

    #[tokio::test]
    async fn filter_by_field_on_raw_details() {
        let (tx, rx) = mpsc::channel(4);
        let inner: Box<dyn EventStream> = Box::new(ChannelStream { rx });
        let mut filter = FilterStream::new(inner, by_field("status", &serde_json::json!("ok")));

        tx.send(make_raw_event(serde_json::json!({"status": "error"})))
            .await
            .unwrap();
        tx.send(make_raw_event(serde_json::json!({"status": "ok"})))
            .await
            .unwrap();
        drop(tx);

        let e = filter.next().await.unwrap();
        assert_eq!(e.details["status"], "ok");
        assert!(filter.next().await.is_none());
    }

    #[tokio::test]
    async fn filter_pass_all_does_not_drop() {
        let (tx, rx) = mpsc::channel(2);
        let inner: Box<dyn EventStream> = Box::new(ChannelStream { rx });
        let mut filter = FilterStream::new(inner, pass_all());

        tx.send(make_event("a", "a.x")).await.unwrap();
        tx.send(make_event("b", "b.y")).await.unwrap();
        drop(tx);

        assert!(filter.next().await.is_some());
        assert!(filter.next().await.is_some());
        assert!(filter.next().await.is_none());
    }

    #[tokio::test]
    async fn filter_propagates_none_when_inner_closes() {
        let (_tx, rx) = mpsc::channel::<Event>(1);
        drop(_tx);
        let inner: Box<dyn EventStream> = Box::new(ChannelStream { rx });
        let mut filter = FilterStream::new(inner, by_source("timer"));
        assert!(filter.next().await.is_none());
    }

    #[tokio::test]
    async fn filter_drops_all_then_inner_closes() {
        let (tx, rx) = mpsc::channel(2);
        let inner: Box<dyn EventStream> = Box::new(ChannelStream { rx });
        let mut filter = FilterStream::new(inner, by_source("nonexistent"));

        tx.send(make_event("timer", "timer.once")).await.unwrap();
        tx.send(make_event("heartbeat", "heartbeat.timeout"))
            .await
            .unwrap();
        drop(tx);

        // All events dropped, then inner closes → None
        assert!(filter.next().await.is_none());
    }
}
