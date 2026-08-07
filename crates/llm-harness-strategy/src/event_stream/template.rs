//! Default structured event payload template.
//!
//! Provides the four-dimension JSON schema (source / channel / event_id /
//! timestamp / context) for `Event.details`, as described in ai-agent-book
//! layer analysis item 41:
//!
//! > "事件结构化模板（来源/渠道/内容/上下文四维度默认 JSON schema）"
//!
//! All built-in EventStream implementations (TimerStream, HeartbeatStream,
//! FilterStream) construct `Event.details` via `EventDetails::new()` so that
//! downstream consumers (FilterStream predicates, business-layer recovery
//! logic) can rely on a consistent shape.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

// ── Types ───────────────────────────────────────────────────────────────

/// Default event payload template. Maps to `Event.details` (a
/// `serde_json::Value`).
///
/// Four dimensions per the tutorial:
/// - `source` — event origin category ("timer", "heartbeat", "shell_monitor", ...)
/// - `channel` — specific channel within source ("timer.once", "heartbeat.timeout", ...)
/// - `event_id` — unique identifier for deduplication
/// - `timestamp` — when the event occurred (UTC)
/// - `context` — optional structured context (label, reason, related task)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDetails {
    /// Event origin category, e.g. `"timer"`, `"heartbeat"`.
    pub source: String,
    /// Specific channel, e.g. `"timer.once"`, `"heartbeat.timeout"`.
    pub channel: String,
    /// Unique event ID for deduplication.
    pub event_id: String,
    /// UTC timestamp of event occurrence.
    pub timestamp: DateTime<Utc>,
    /// Optional structured context.
    #[serde(default)]
    pub context: EventContext,
}

/// Optional context fields embedded in [`EventDetails`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventContext {
    /// Human-readable label for the event source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Why this event was triggered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_reason: Option<String>,
    /// Related task ID, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_task_id: Option<String>,
}

impl EventDetails {
    /// Create a new event details with the given source and channel.
    /// Generates a UUID v4 event_id and current UTC timestamp.
    pub fn new(source: &str, channel: &str) -> Self {
        Self {
            source: source.to_string(),
            channel: channel.to_string(),
            event_id: Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            context: EventContext::default(),
        }
    }

    /// Set a human-readable label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.context.label = Some(label.into());
        self
    }

    /// Set the trigger reason.
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.context.trigger_reason = Some(reason.into());
        self
    }

    /// Set the related task ID.
    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.context.related_task_id = Some(task_id.into());
        self
    }

    /// Serialize to `serde_json::Value` for use as `Event.details`.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
    }

    /// Deserialize from `Event.details` value.
    /// Returns `None` if the value doesn't match the schema.
    pub fn from_value(v: &Value) -> Option<Self> {
        serde_json::from_value::<EventDetails>(v.clone()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_details_roundtrip() {
        let details = EventDetails::new("timer", "timer.once")
            .with_label("poll-ci")
            .with_reason("scheduled fire");
        let val = details.to_value();
        assert_eq!(val["source"], "timer");
        assert_eq!(val["channel"], "timer.once");
        assert!(val["event_id"].is_string());
        assert!(val["timestamp"].is_string());
        assert_eq!(val["context"]["label"], "poll-ci");
        assert_eq!(val["context"]["trigger_reason"], "scheduled fire");
        assert!(val["context"]["related_task_id"].is_null());

        let parsed = EventDetails::from_value(&val).unwrap();
        assert_eq!(parsed.source, "timer");
        assert_eq!(parsed.channel, "timer.once");
        assert_eq!(parsed.context.label.as_deref(), Some("poll-ci"));
    }

    #[test]
    fn event_details_minimal_omits_null_context_fields() {
        let details = EventDetails::new("heartbeat", "heartbeat.timeout");
        let val = details.to_value();
        // context exists but label/reason/task_id are omitted
        assert!(val["context"].is_object());
        assert!(val["context"]["label"].is_null());
    }

    #[test]
    fn event_details_from_value_rejects_invalid() {
        assert!(EventDetails::from_value(&serde_json::json!("not an object")).is_none());
        assert!(EventDetails::from_value(&serde_json::json!({"foo": 1})).is_none());
    }

    #[test]
    fn event_details_event_id_is_unique() {
        let d1 = EventDetails::new("timer", "timer.once");
        let d2 = EventDetails::new("timer", "timer.once");
        assert_ne!(d1.event_id, d2.event_id);
    }
}
