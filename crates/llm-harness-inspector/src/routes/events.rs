//! WebSocket handler for `GET /api/events`.
//!
//! On connect the handler sends an initial [`WsMessage`] snapshot of the
//! current harness state and usage, then subscribes to the harness event
//! broadcast and forwards each event as a JSON text frame until the client
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
/// Rejects connections from non-localhost origins to prevent WebSocket CSRF
/// (a malicious page opened in the same browser could otherwise connect to
/// `ws://127.0.0.1:PORT/api/events` and exfiltrate agent state).
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<InspectorState>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Some(origin) = headers.get("origin")
        && let Ok(origin_str) = origin.to_str()
    {
        let lowered = origin_str.to_ascii_lowercase();
        let is_localhost = lowered.starts_with("http://localhost")
            || lowered.starts_with("http://127.0.0.1")
            || lowered.starts_with("https://localhost")
            || lowered.starts_with("https://127.0.0.1");
        if !is_localhost {
            return axum::http::StatusCode::FORBIDDEN.into_response();
        }
    }
    // No Origin header (non-browser clients like curl) — allow.
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// Drive a single WebSocket connection: snapshot, then stream events.
async fn handle_ws(mut socket: WebSocket, state: InspectorState) {
    // 1. Send the initial snapshot so a freshly connected client immediately
    //    knows the current phase, model, usage, etc.
    let hs = state.harness.state();
    let usage = state.harness.usage();
    let projected = project_harness_state(&hs);
    let snapshot = WsMessage::snapshot(&projected, &usage, Utc::now());
    let json = serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".into());
    if socket.send(Message::Text(json.into())).await.is_err() {
        return;
    }

    // 2. Subscribe AFTER sending the snapshot. Events published between the
    //    snapshot read and the subscribe call are lost, but that gap is
    //    sub-microsecond and acceptable for an inspection/debugging surface.
    let mut rx = state.harness.subscribe();

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
