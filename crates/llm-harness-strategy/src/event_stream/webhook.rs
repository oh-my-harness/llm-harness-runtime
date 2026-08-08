//! Webhook receiver event stream — external channel via mpsc.
//!
//! Implements item 40 (外部通道 webhook 接收器默认实现).
//!
//! Business layer receives HTTP requests (or other external events) and
//! pushes payloads via `WebhookChannel::push()`. `WebhookStream` implements
//! `EventStream` for `WaitForExternalEventTool`.
//!
//! # Transparent passthrough
//!
//! Payload is passed directly as `Event.details`. Content is
//! `DataBlock::text(payload.to_string())`. If the business layer wants
//! `EventDetails` structure (for `FilterStream` compatibility), they must
//! construct it before pushing — see `EventDetails::new("webhook", ...).to_value()`.
//!
//! # FilterStream compatibility
//!
//! [`FilterStream::by_source`](super::filter::by_source) and
//! [`by_channel`](super::filter::by_channel) rely on
//! `EventDetails::from_value()`, which requires `Event.details` to match
//! the `EventDetails` schema (a JSON object with `source`, `channel`, etc.).
//! Arbitrary webhook payloads that don't match this schema will be silently
//! dropped by these filters. To use filtering, push
//! `EventDetails::new("webhook", "...").to_value()` as the payload, or use
//! [`by_field`](super::filter::by_field) which works on arbitrary JSON.

use futures::future::BoxFuture;
use serde_json::Value;

use llm_harness_types::{DataBlock, Event, EventStream};

/// Lightweight sender handle. `Clone + Send + Sync`.
///
/// Business layer holds this and calls `push()` when an external event
/// arrives (e.g. HTTP webhook received).
#[derive(Clone)]
pub struct WebhookChannel {
    tx: tokio::sync::mpsc::Sender<Value>,
}

impl WebhookChannel {
    /// Push a payload into the stream.
    ///
    /// Returns `Err` if the stream has been dropped. Blocks (applies
    /// backpressure) if the buffer is full.
    pub async fn push(
        &self,
        payload: Value,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<Value>> {
        self.tx.send(payload).await
    }
}

/// Webhook receiver event stream.
///
/// Wraps `mpsc::Receiver<Value>`. Each received payload becomes an `Event`
/// with transparent passthrough (payload as `details`, stringified as
/// `content`).
pub struct WebhookStream {
    rx: tokio::sync::mpsc::Receiver<Value>,
}

impl WebhookStream {
    /// Create a new webhook stream. Returns `(channel, boxed stream)`.
    ///
    /// - `buffer`: mpsc channel capacity. 64 is a reasonable default.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(buffer: usize) -> (WebhookChannel, Box<dyn EventStream>) {
        let (tx, rx) = tokio::sync::mpsc::channel(buffer);
        let channel = WebhookChannel { tx };
        let stream = WebhookStream { rx };
        (channel, Box::new(stream))
    }
}

impl EventStream for WebhookStream {
    fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<Event>> {
        Box::pin(async move {
            self.rx.recv().await.map(|payload| Event {
                content: vec![DataBlock::text(payload.to_string())],
                details: payload,
            })
        })
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn push_and_receive() {
        let (channel, mut stream) = WebhookStream::new(64);
        channel
            .push(serde_json::json!({"event": "pushed"}))
            .await
            .unwrap();

        let event = stream.next().await.unwrap();
        assert_eq!(event.details["event"], "pushed");
    }

    #[tokio::test]
    async fn transparent_passthrough() {
        let (channel, mut stream) = WebhookStream::new(64);
        let payload = serde_json::json!({
            "source": "github",
            "action": "opened",
            "pr": 42
        });
        channel.push(payload.clone()).await.unwrap();

        let event = stream.next().await.unwrap();
        // details is the payload verbatim
        assert_eq!(event.details, payload);
        // content is payload.to_string()
        let text = match &event.content[0] {
            llm_harness_types::DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text block"),
        };
        assert_eq!(text, payload.to_string());
    }

    #[tokio::test]
    async fn channel_closed_returns_none() {
        let (channel, mut stream) = WebhookStream::new(64);
        drop(channel);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn multiple_pushes_in_order() {
        let (channel, mut stream) = WebhookStream::new(64);
        channel.push(serde_json::json!({"seq": 1})).await.unwrap();
        channel.push(serde_json::json!({"seq": 2})).await.unwrap();

        let e1 = stream.next().await.unwrap();
        assert_eq!(e1.details["seq"], 1);

        let e2 = stream.next().await.unwrap();
        assert_eq!(e2.details["seq"], 2);
    }

    #[tokio::test]
    async fn buffer_capacity_push_returns_err_when_full() {
        let (channel, mut stream) = WebhookStream::new(1);
        // First push fills the buffer (capacity=1)
        channel
            .push(serde_json::json!({"first": true}))
            .await
            .unwrap();
        // With the buffer full and no receiver draining, a second push
        // blocks rather than erroring — mpsc::send().await parks.
        // Use try_send on the underlying sender to verify backpressure.
        let result = channel
            .tx
            .clone()
            .try_send(serde_json::json!({"second": true}));
        assert!(result.is_err());
        // Drain to prove the first one is there
        let e = stream.next().await.unwrap();
        assert_eq!(e.details["first"], true);
    }

    #[tokio::test]
    async fn channel_is_clone_and_send() {
        let (channel, _stream) = WebhookStream::new(64);
        let c2 = channel.clone();
        let c3 = channel.clone();

        let join = tokio::spawn(async move {
            c2.push(serde_json::json!({"from": "c2"})).await.unwrap();
            c3.push(serde_json::json!({"from": "c3"})).await.unwrap();
        });
        join.await.unwrap();
        channel
            .push(serde_json::json!({"from": "original"}))
            .await
            .unwrap();
    }
}
