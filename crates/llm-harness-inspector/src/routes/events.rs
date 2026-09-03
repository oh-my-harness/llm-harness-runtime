//! WebSocket handler for `GET /api/events`.
//!
//! On connect the handler subscribes to the harness event broadcast, then
//! sends an initial [`WsMessage`] snapshot of the current harness state and
//! usage, then forwards each event as a JSON text frame until the client
//! disconnects or the broadcast sender is dropped.

use crate::event_ser::{SerializableAgentHarnessEvent, WsMessage, project_harness_state};
use crate::state::InspectorState;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use futures::sink::SinkExt;
use tokio::sync::broadcast::error::RecvError;

/// Upgrade the HTTP request to a WebSocket connection.
///
/// Requires a `token` query parameter matching the shared secret generated
/// when the inspector server was mounted. WebSocket requests cannot always
/// carry headers, and browser origin checks are not sufficient to prevent a
/// malicious same-host page from using a live local session.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<InspectorState>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let token = query.get("token").map(String::as_str).unwrap_or("");
    if token.is_empty() || !state.token.matches(token) {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    // Reject non-localhost browser origins to prevent WebSocket CSRF.
    if let Some(origin) = headers.get("origin")
        && let Ok(origin_str) = origin.to_str()
    {
        let lowered = origin_str.to_ascii_lowercase();
        if !lowered.starts_with("http://localhost")
            && !lowered.starts_with("http://127.0.0.1")
            && !lowered.starts_with("https://localhost")
            && !lowered.starts_with("https://127.0.0.1")
        {
            return axum::http::StatusCode::FORBIDDEN.into_response();
        }
    }
    let agent = query.get("agent").cloned();
    ws.on_upgrade(move |socket| handle_ws(socket, state, agent))
}

/// Serve a virtual agent (no harness): snapshot = buffered shared lines,
/// then live-forward new ones.
async fn handle_shared_ws(
    mut socket: WebSocket,
    shared: std::sync::Arc<crate::state::SharedEvents>,
    name: &str,
) {
    // Snapshot: buffered lines for this agent.
    let buffered: Vec<serde_json::Value> = shared
        .replay()
        .into_iter()
        .filter(|l| l.get("agent").and_then(|a| a.as_str()) == Some(name))
        .collect();
    let snapshot = WsMessage {
        r#type: "Snapshot",
        inner_type: None,
        timestamp: Utc::now(),
        data: serde_json::json!({
            "state": { "phase": "Shared", "model": "(virtual)" },
            "usage": {},
        }),
    };
    if socket
        .send(Message::Text(
            serde_json::to_string(&snapshot)
                .unwrap_or_else(|_| "{}".into())
                .into(),
        ))
        .await
        .is_err()
    {
        return;
    }
    let history = WsMessage {
        r#type: "History",
        inner_type: None,
        timestamp: Utc::now(),
        data: serde_json::json!({ "shared": buffered }),
    };
    if socket
        .send(Message::Text(
            serde_json::to_string(&history)
                .unwrap_or_else(|_| "{}".into())
                .into(),
        ))
        .await
        .is_err()
    {
        return;
    }
    let mut rx = shared.subscribe();
    loop {
        match rx.recv().await {
            Ok(line) => {
                if line.get("agent").and_then(|a| a.as_str()) != Some(name) {
                    continue;
                }
                let msg = WsMessage {
                    r#type: "SharedLine",
                    inner_type: None,
                    timestamp: Utc::now(),
                    data: line,
                };
                let json = serde_json::to_string(&msg).unwrap_or_else(|_| "{}".into());
                if socket.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        }
    }
    let _ = socket.close().await;
}

/// Drive a single WebSocket connection: subscribe first, then send snapshot,
/// then stream events. Subscribing before reading the snapshot ensures no
/// events are lost between the snapshot read and the subscribe call.
async fn handle_ws(mut socket: WebSocket, state: InspectorState, agent: Option<String>) {
    // Virtual agents (e.g. "operator") are served entirely from SharedEvents.
    if let Some(shared) = state.shared.as_ref()
        && let Some(name) = agent.as_deref()
        && state.registry.get(Some(name)).is_none()
    {
        return handle_shared_ws(socket, shared.clone(), name).await;
    }
    let Ok(harness) = state.resolve(agent.as_deref()) else {
        let _ = socket
            .send(Message::Text(
                serde_json::json!({ "type": "Error", "error": "unknown agent" })
                    .to_string()
                    .into(),
            ))
            .await;
        let _ = socket.close().await;
        return;
    };
    // 1. Subscribe FIRST — events arriving while we read the snapshot will
    //    be buffered in the broadcast channel and delivered after the
    //    snapshot, which is correct ordering for a debugging surface.
    let mut rx = harness.subscribe();

    // 2. Send the initial snapshot so a freshly connected client immediately
    //    knows the current phase, model, usage, etc.
    let hs = harness.state();
    let usage = harness.usage();
    let projected = project_harness_state(&hs);
    let snapshot = WsMessage::snapshot(&projected, &usage, Utc::now());
    let json = serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".into());
    if socket.send(Message::Text(json.into())).await.is_err() {
        return;
    }

    // 2b. History replay: render the session's active path so a freshly
    // connected (or agent-switched) client sees prior conversation.
    if let Ok(entries) = harness.read_active_path().await {
        let history = WsMessage::history(&entries);
        let json = serde_json::to_string(&history).unwrap_or_else(|_| "{}".into());
        if socket.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }

    // 3. Forward events until the client disconnects or the broadcast closes.
    loop {
        match rx.recv().await {
            Ok(event) => {
                let projected = SerializableAgentHarnessEvent::from(&*event);
                let msg = WsMessage::from_event(&projected, Utc::now());
                let json = serde_json::to_string(&msg).unwrap_or_else(|_| "{}".into());
                if socket.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        }
    }

    let _ = socket.close().await;
}
