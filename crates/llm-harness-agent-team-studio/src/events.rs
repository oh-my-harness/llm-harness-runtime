//! Project-level event bus feeding the panel's WebSocket stream.
//!
//! Mirrors the inspector's `SharedEvents` pattern: a broadcast channel
//! for live fans-out plus a bounded replay buffer so a freshly connected
//! panel sees recent context. The binary pumps each project's operator
//! stream into `publish_operator`; issue transitions publish directly
//! from the route handlers.

use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::Arc;

/// How many recent events a new WS client sees on connect.
const REPLAY_LEN: usize = 50;

#[derive(Clone)]
pub struct EventBus {
    tx: tokio::sync::broadcast::Sender<String>,
    buffer: Arc<RwLock<VecDeque<String>>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(256);
        Self {
            tx,
            buffer: Arc::new(RwLock::new(VecDeque::with_capacity(REPLAY_LEN))),
        }
    }

    /// Publish one event (already-serialized JSON string). Buffered for
    /// replay and broadcast to live subscribers. No subscribers is fine.
    pub fn publish(&self, event: serde_json::Value) {
        let line = event.to_string();
        self.buffer.write().push_back(line.clone());
        if self.buffer.read().len() > REPLAY_LEN {
            self.buffer.write().pop_front();
        }
        let _ = self.tx.send(line);
    }

    /// Recent events for a new connection (oldest first).
    pub fn replay(&self) -> Vec<String> {
        self.buffer.read().iter().cloned().collect()
    }

    /// Drop buffered events for a project; used after deleting a team so a
    /// recreated team with the same ID does not inherit old chat context.
    pub fn clear_project(&self, project: &str) {
        let project = project.to_string();
        self.buffer.write().retain(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|event| {
                    event
                        .get("project")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .as_deref()
                != Some(project.as_str())
        });
    }

    /// Subscribe to live events.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.tx.subscribe()
    }
}

/// Build one operator-message event.
/// Agent 错误事件：成员调 LLM/工具失败时推给面板（WS + 历史缓冲）。
pub fn agent_error_event(project: &str, agent: &str, error: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "agent_error",
        "project": project,
        "agent": agent,
        "text": error,
        "ts": chrono::Utc::now().to_rfc3339(),
    })
}

pub fn operator_event(project: &str, from: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "operator_msg",
        "project": project,
        "from": from,
        "text": text,
        "ts": chrono::Utc::now().to_rfc3339(),
    })
}

/// Build one member thinking-delta event for the live team panel.
pub fn agent_thought_event(
    project: &str,
    agent: &str,
    message_id: &str,
    text: &str,
) -> serde_json::Value {
    serde_json::json!({
        "type": "agent_thought",
        "project": project,
        "agent": agent,
        "message_id": message_id,
        "text": text,
        "ts": chrono::Utc::now().to_rfc3339(),
    })
}

/// Build one chat-message event for the WeChat-style panel.
#[allow(clippy::too_many_arguments)]
pub fn team_message_event(
    project: &str,
    from: &str,
    to: &str,
    text: &str,
    message_type: &str,
    message_id: Option<String>,
    broadcast_id: Option<String>,
    view_kind: &str,
) -> serde_json::Value {
    let conversation =
        if view_kind == "direct_message" && from == llm_harness_agent_team::reserved::OPERATOR {
            to
        } else if view_kind == "direct_message" {
            from
        } else {
            "team"
        };
    serde_json::json!({
        "type": "team_message",
        "project": project,
        "from": from,
        "to": to,
        "text": text,
        "message_type": message_type,
        "message_id": message_id,
        "broadcast_id": broadcast_id,
        "view_kind": view_kind,
        "conversation": conversation,
        "ts": chrono::Utc::now().to_rfc3339(),
    })
}

/// Build one issue-changed event.
pub fn issue_event(project: &str, id: &str, status: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "issue_changed",
        "project": project,
        "id": id,
        "status": status,
        "ts": chrono::Utc::now().to_rfc3339(),
    })
}

pub fn team_deleted_event(project: &str) -> serde_json::Value {
    let ts = chrono::Utc::now().to_rfc3339();
    serde_json::json!({
        "type": "team_deleted",
        "project": project,
        "ts": ts,
    })
}

pub fn team_delete_failed_event(project: &str, error: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "team_delete_failed",
        "project": project,
        "text": error,
        "ts": chrono::Utc::now().to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_buffers_and_fans_out() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(operator_event("p", "planner", "hello"));
        let got = rx.try_recv().unwrap();
        assert!(got.contains("hello"));
        assert_eq!(bus.replay().len(), 1);
    }

    #[test]
    fn replay_caps_at_50() {
        let bus = EventBus::new();
        for i in 0..60 {
            bus.publish(operator_event("p", "a", &i.to_string()));
        }
        let replay = bus.replay();
        assert_eq!(replay.len(), 50);
        assert!(
            replay[0].contains("\"10\""),
            "oldest evicted: {:?}",
            replay[0]
        );
    }

    #[test]
    fn publish_without_subscribers_ok() {
        let bus = EventBus::new();
        bus.publish(operator_event("p", "a", "x"));
        assert_eq!(bus.replay().len(), 1);
    }

    #[test]
    fn delete_failure_event_is_actionable() {
        let event = team_delete_failed_event("p1", "directory is locked");
        assert_eq!(event["type"], "team_delete_failed");
        assert_eq!(event["project"], "p1");
        assert_eq!(event["text"], "directory is locked");
    }
}
