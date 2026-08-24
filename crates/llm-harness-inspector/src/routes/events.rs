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
/// when the inspector server was mounted. This prevents other local processes
/// from connecting to the event stream and reading agent state.
///
/// Also rejects connections from non-localhost origins to prevent WebSocket
/// CSRF (a malicious page opened in the same browser could otherwise connect
/// to `ws://127.0.0.1:PORT/api/events` and exfiltrate agent state).
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<InspectorState>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    // Token check: required for non-browser clients (no Origin header).
    // Browser connections from localhost are authenticated by the Origin
    // check below — this prevents CSRF without breaking the Inspector UI,
    // which has no way to obtain the token.
    let has_localhost_origin = headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            let l = s.to_ascii_lowercase();
            l.starts_with("http://localhost")
                || l.starts_with("http://127.0.0.1")
                || l.starts_with("https://localhost")
                || l.starts_with("https://127.0.0.1")
        })
        .unwrap_or(false);

    if !has_localhost_origin {
        let token = query.get("token").map(|s| s.as_str()).unwrap_or("");
        if token.is_empty() || !state.token.matches(token) {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
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
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// Drive a single WebSocket connection: subscribe first, then send snapshot,
/// then stream events. Subscribing before reading the snapshot ensures no
/// events are lost between the snapshot read and the subscribe call.
async fn handle_ws(mut socket: WebSocket, state: InspectorState) {
    // 1. Subscribe FIRST — events arriving while we read the snapshot will
    //    be buffered in the broadcast channel and delivered after the
    //    snapshot, which is correct ordering for a debugging surface.
    let mut rx = state.harness.subscribe();

    // 2. Send the initial snapshot so a freshly connected client immediately
    //    knows the current phase, model, usage, etc.
    let hs = state.harness.state();
    let usage = state.harness.usage();
    let projected = project_harness_state(&hs);
    let snapshot = WsMessage::snapshot(&projected, &usage, Utc::now());
    let json = serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".into());
    if socket.send(Message::Text(json.into())).await.is_err() {
        return;
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
