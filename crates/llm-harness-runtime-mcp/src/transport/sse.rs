use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::sse_codec::{SseDecoder, SseEvent};
use super::{
    CloseHandler, McpTransport, McpTransportError, NotificationHandler, RequestHandler,
    TransportCallbacks, TransportResult, headers_from_config, request_error_response,
    resolve_timeout_ms,
};
use crate::config::McpServerConfig;
use crate::json_rpc::{
    JsonRpcError, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RequestId,
    error_codes,
};

const MCP_PROTOCOL_VERSION: &str = "mcp-protocol-version";
type PendingResponse = oneshot::Sender<TransportResult<Value>>;

struct LegacySseInner {
    client: reqwest::Client,
    source_url: Url,
    endpoint: RwLock<Option<Url>>,
    headers: HashMap<String, String>,
    protocol_version: RwLock<Option<String>>,
    connected: AtomicBool,
    closing: AtomicBool,
    next_id: AtomicU64,
    timeout_ms: u64,
    callbacks: TransportCallbacks,
    pending: Mutex<HashMap<RequestId, PendingResponse>>,
    cancellation: CancellationToken,
    listener_task: Mutex<Option<JoinHandle<()>>>,
}

impl LegacySseInner {
    fn request_headers(&self, accept: &'static str) -> TransportResult<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static(accept));
        for (name, value) in &self.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                McpTransportError::InvalidConfig(format!(
                    "invalid HTTP header name '{name}': {error}"
                ))
            })?;
            let value = HeaderValue::from_str(value).map_err(|error| {
                McpTransportError::InvalidConfig(format!(
                    "invalid HTTP header value for '{name}': {error}"
                ))
            })?;
            headers.insert(name, value);
        }
        if let Some(version) = self.protocol_version.read().as_deref() {
            headers.insert(
                HeaderName::from_static(MCP_PROTOCOL_VERSION),
                HeaderValue::from_str(version).map_err(|error| {
                    McpTransportError::InvalidConfig(format!(
                        "invalid MCP protocol version header: {error}"
                    ))
                })?,
            );
        }
        Ok(headers)
    }

    async fn post_json<T: Serialize + ?Sized>(&self, value: &T) -> TransportResult<()> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(McpTransportError::Disconnected);
        }
        let endpoint = self.endpoint.read().clone().ok_or_else(|| {
            McpTransportError::InvalidConfig("SSE endpoint was not received".into())
        })?;
        let response = self
            .client
            .post(endpoint)
            .headers(self.request_headers("application/json")?)
            .header(CONTENT_TYPE, "application/json")
            .json(value)
            .send()
            .await
            .map_err(|error| McpTransportError::Http {
                status: 0,
                body: error.to_string(),
            })?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Err(McpTransportError::Http { status, body })
    }

    fn fail_pending(&self, error: McpTransportError) {
        let pending = std::mem::take(&mut *self.pending.lock());
        for (_, sender) in pending {
            let _ = sender.send(Err(error.clone()));
        }
    }

    fn mark_closed(&self) {
        if self.connected.swap(false, Ordering::AcqRel) {
            self.fail_pending(McpTransportError::Closed);
            if !self.closing.load(Ordering::Acquire) {
                self.callbacks.close();
            }
        }
    }

    async fn handle_message(self: &Arc<Self>, message: JsonRpcMessage) {
        match message {
            JsonRpcMessage::Response(response) => self.handle_response(response),
            JsonRpcMessage::Notification(notification) => self.callbacks.notify(notification),
            JsonRpcMessage::Request(request) => {
                let inner = Arc::clone(self);
                tokio::spawn(async move {
                    inner.handle_server_request(request).await;
                });
            }
        }
    }

    fn handle_response(&self, response: JsonRpcResponse) {
        let Some(ref id) = response.id else {
            tracing::warn!("ignoring legacy SSE response without request id");
            return;
        };
        let Some(sender) = self.pending.lock().remove(id) else {
            tracing::debug!(?id, "ignoring legacy SSE response for unknown request id");
            return;
        };
        let _ = sender.send(response.into_result().map_err(McpTransportError::from));
    }

    async fn handle_server_request(self: &Arc<Self>, request: JsonRpcRequest) {
        let id = request.id.clone();
        let response = match self.callbacks.request() {
            Some(handler) => match handler(request).await {
                Ok(result) => JsonRpcResponse::success(id, result),
                Err(error) => request_error_response(id, error),
            },
            None => JsonRpcResponse::error(
                Some(id),
                JsonRpcError {
                    code: error_codes::METHOD_NOT_FOUND,
                    message: "Method not found".to_owned(),
                    data: None,
                },
            ),
        };
        if let Err(error) = self.post_json(&response).await {
            tracing::warn!(%error, "failed to reply to legacy SSE server request");
        }
    }
}

/// The legacy MCP HTTP+SSE transport used by protocol versions before
/// Streamable HTTP.
pub struct LegacySseTransport {
    inner: Arc<LegacySseInner>,
}

impl LegacySseTransport {
    pub async fn connect(config: &McpServerConfig) -> TransportResult<Self> {
        let McpServerConfig::HttpOrSse { url, timeout, .. } = config else {
            return Err(McpTransportError::InvalidConfig(
                "expected SSE server config".to_owned(),
            ));
        };
        if !config.is_sse() {
            return Err(McpTransportError::InvalidConfig(
                "legacy SSE transport requires type 'sse'".to_owned(),
            ));
        }
        config
            .validate()
            .map_err(|error| McpTransportError::InvalidConfig(error.to_string()))?;

        let source_url =
            Url::parse(url).map_err(|error| McpTransportError::InvalidConfig(error.to_string()))?;
        let inner = Arc::new(LegacySseInner {
            client: reqwest::Client::new(),
            source_url: source_url.clone(),
            endpoint: RwLock::new(None),
            headers: headers_from_config(config)?,
            protocol_version: RwLock::new(None),
            connected: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            timeout_ms: resolve_timeout_ms(
                std::env::var("OMP_MCP_TIMEOUT_MS").ok().as_deref(),
                *timeout,
            ),
            callbacks: TransportCallbacks::default(),
            pending: Mutex::new(HashMap::new()),
            cancellation: CancellationToken::new(),
            listener_task: Mutex::new(None),
        });

        let response = inner
            .client
            .get(source_url)
            .headers(inner.request_headers("text/event-stream")?)
            .send()
            .await
            .map_err(|error| McpTransportError::Http {
                status: 0,
                body: error.to_string(),
            })?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(McpTransportError::Http { status, body });
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type.starts_with("text/event-stream") {
            return Err(McpTransportError::InvalidConfig(format!(
                "legacy SSE endpoint returned content type '{content_type}'"
            )));
        }

        inner.connected.store(true, Ordering::Release);
        let (endpoint_sender, endpoint_receiver) = oneshot::channel();
        let weak = Arc::downgrade(&inner);
        *inner.listener_task.lock() = Some(tokio::spawn(read_event_stream(
            weak,
            response,
            Some(endpoint_sender),
        )));

        let wait_for_endpoint = async {
            endpoint_receiver.await.map_err(|_| {
                McpTransportError::InvalidConfig(
                    "legacy SSE connection closed before endpoint event".to_owned(),
                )
            })?
        };
        let endpoint_result = if inner.timeout_ms == 0 {
            wait_for_endpoint.await
        } else {
            tokio::time::timeout(Duration::from_millis(inner.timeout_ms), wait_for_endpoint)
                .await
                .map_err(|_| McpTransportError::Timeout {
                    method: "SSE endpoint discovery".to_owned(),
                    timeout_ms: inner.timeout_ms,
                })?
        };
        if let Err(error) = endpoint_result {
            inner.mark_closed();
            if let Some(handle) = inner.listener_task.lock().take() {
                handle.abort();
            }
            return Err(error);
        }

        Ok(Self { inner })
    }
}

#[async_trait]
impl McpTransport for LegacySseTransport {
    async fn request(&self, method: &str, params: Value) -> TransportResult<Value> {
        if !self.is_connected() {
            return Err(McpTransportError::Disconnected);
        }
        let id = RequestId::from(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let request = JsonRpcRequest::new(id.clone(), method, params);
        let (sender, receiver) = oneshot::channel();
        self.inner.pending.lock().insert(id.clone(), sender);

        if let Err(error) = self.inner.post_json(&request).await {
            self.inner.pending.lock().remove(&id);
            return Err(error);
        }

        let receive = async { receiver.await.unwrap_or(Err(McpTransportError::Closed)) };
        if self.inner.timeout_ms == 0 {
            receive.await
        } else {
            tokio::time::timeout(Duration::from_millis(self.inner.timeout_ms), receive)
                .await
                .map_err(|_| {
                    self.inner.pending.lock().remove(&id);
                    McpTransportError::Timeout {
                        method: method.to_owned(),
                        timeout_ms: self.inner.timeout_ms,
                    }
                })?
        }
    }

    async fn notify(&self, method: &str, params: Value) -> TransportResult<()> {
        self.inner
            .post_json(&JsonRpcNotification::new(method, params))
            .await
    }

    fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::Acquire)
    }

    async fn close(&self) -> TransportResult<()> {
        if !self.inner.connected.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        self.inner.closing.store(true, Ordering::Release);
        self.inner.cancellation.cancel();
        self.inner.fail_pending(McpTransportError::Closed);
        if let Some(handle) = self.inner.listener_task.lock().take() {
            handle.abort();
        }
        self.inner.callbacks.close();
        Ok(())
    }

    fn set_protocol_version(&self, version: &str) {
        *self.inner.protocol_version.write() = Some(version.to_owned());
    }

    fn on_request(&self, handler: RequestHandler) {
        self.inner.callbacks.set_request(handler);
    }

    fn on_notification(&self, handler: NotificationHandler) {
        self.inner.callbacks.set_notification(handler);
    }

    fn on_close(&self, handler: CloseHandler) {
        self.inner.callbacks.set_close(handler);
    }

    fn clear_on_close(&self) {
        self.inner.callbacks.clear_close();
    }
}

impl Drop for LegacySseTransport {
    fn drop(&mut self) {
        self.inner.closing.store(true, Ordering::Release);
        self.inner.cancellation.cancel();
        if let Some(handle) = self.inner.listener_task.lock().take() {
            handle.abort();
        }
    }
}

async fn read_event_stream(
    weak: Weak<LegacySseInner>,
    mut response: reqwest::Response,
    mut endpoint_sender: Option<oneshot::Sender<TransportResult<()>>>,
) {
    let mut decoder = SseDecoder::default();
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        tokio::select! {
            _ = inner.cancellation.cancelled() => return,
            chunk = response.chunk() => {
                drop(inner);
                match chunk {
                    Ok(Some(chunk)) => match decoder.push(&chunk) {
                        Ok(events) => {
                            for event in events {
                                handle_sse_event(&weak, event, &mut endpoint_sender).await;
                            }
                        }
                        Err(error) => {
                            if let Some(sender) = endpoint_sender.take() {
                                let _ = sender.send(Err(McpTransportError::Json(error.to_string())));
                            }
                            tracing::warn!(%error, "legacy SSE stream was not UTF-8");
                            break;
                        }
                    },
                    Ok(None) => {
                        if let Some(event) = decoder.finish() {
                            handle_sse_event(&weak, event, &mut endpoint_sender).await;
                        }
                        break;
                    }
                    Err(error) => {
                        if let Some(sender) = endpoint_sender.take() {
                            let _ = sender.send(Err(McpTransportError::Http {
                                status: 0,
                                body: error.to_string(),
                            }));
                        }
                        tracing::warn!(%error, "legacy SSE stream read failed");
                        break;
                    }
                }
            }
        }
    }
    if let Some(sender) = endpoint_sender.take() {
        let _ = sender.send(Err(McpTransportError::InvalidConfig(
            "legacy SSE connection closed before endpoint event".to_owned(),
        )));
    }
    if let Some(inner) = weak.upgrade() {
        inner.mark_closed();
    }
}

async fn handle_sse_event(
    weak: &Weak<LegacySseInner>,
    event: SseEvent,
    endpoint_sender: &mut Option<oneshot::Sender<TransportResult<()>>>,
) {
    let Some(inner) = weak.upgrade() else {
        return;
    };
    match event.event.as_deref() {
        Some("endpoint") => {
            let result = inner
                .source_url
                .join(event.data.trim())
                .map_err(|error| McpTransportError::InvalidConfig(error.to_string()));
            match result {
                Ok(endpoint) => {
                    *inner.endpoint.write() = Some(endpoint);
                    if let Some(sender) = endpoint_sender.take() {
                        let _ = sender.send(Ok(()));
                    }
                }
                Err(error) => {
                    if let Some(sender) = endpoint_sender.take() {
                        let _ = sender.send(Err(error));
                    }
                }
            }
        }
        Some("message") | None => match serde_json::from_str::<JsonRpcMessage>(&event.data) {
            Ok(message) => inner.handle_message(message).await,
            Err(error) => {
                tracing::warn!(%error, data = event.data, "ignoring malformed legacy SSE message");
            }
        },
        Some(kind) => {
            tracing::debug!(event = kind, "ignoring unknown legacy SSE event");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_event_supports_relative_urls() {
        let source = Url::parse("https://example.com/events/stream").unwrap();
        assert_eq!(
            source.join("../messages?id=1").unwrap().as_str(),
            "https://example.com/messages?id=1"
        );
    }
}
