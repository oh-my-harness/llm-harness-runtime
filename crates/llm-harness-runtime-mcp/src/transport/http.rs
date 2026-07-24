use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use parking_lot::{Mutex, RwLock};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, Response, StatusCode};
use serde::Serialize;
use serde_json::Value;
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

const MCP_SESSION_ID: &str = "mcp-session-id";
const MCP_PROTOCOL_VERSION: &str = "mcp-protocol-version";
const MAX_SSE_RESUME_RETRIES: usize = 10;

pub type AuthRefresh =
    Arc<dyn Fn() -> BoxFuture<'static, TransportResult<HashMap<String, String>>> + Send + Sync>;

struct HttpInner {
    url: Url,
    client: reqwest::Client,
    headers: RwLock<HashMap<String, String>>,
    session_id: RwLock<Option<String>>,
    protocol_version: RwLock<Option<String>>,
    connected: AtomicBool,
    closing: AtomicBool,
    next_id: AtomicU64,
    timeout_ms: u64,
    callbacks: TransportCallbacks,
    cancellation: CancellationToken,
    listener_task: Mutex<Option<JoinHandle<()>>>,
    auth_refresh: RwLock<Option<AuthRefresh>>,
}

impl HttpInner {
    fn request_headers(
        &self,
        accept: &'static str,
        last_event_id: Option<&str>,
    ) -> TransportResult<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static(accept));
        for (name, value) in self.headers.read().iter() {
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
        if let Some(session_id) = self.session_id.read().as_deref() {
            headers.insert(
                HeaderName::from_static(MCP_SESSION_ID),
                HeaderValue::from_str(session_id).map_err(|error| {
                    McpTransportError::InvalidConfig(format!(
                        "invalid MCP session id header: {error}"
                    ))
                })?,
            );
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
        if let Some(last_event_id) = last_event_id {
            headers.insert(
                HeaderName::from_static("last-event-id"),
                HeaderValue::from_str(last_event_id).map_err(|error| {
                    McpTransportError::InvalidConfig(format!("invalid SSE Last-Event-ID: {error}"))
                })?,
            );
        }
        Ok(headers)
    }

    /// Mark the transport as closed and notify the manager via the close
    /// callback, unless an explicit `close()` is already in progress.
    fn mark_closed(&self) {
        if self.connected.swap(false, Ordering::AcqRel)
            && !self.closing.load(Ordering::Acquire)
        {
            self.callbacks.close();
        }
    }

    fn capture_session_id(&self, response: &Response) {
        let Some(value) = response.headers().get(MCP_SESSION_ID) else {
            return;
        };
        match value.to_str() {
            Ok(value) => {
                *self.session_id.write() = Some(value.to_owned());
            }
            Err(error) => tracing::warn!(%error, "ignoring invalid MCP session id header"),
        }
    }

    async fn send_json<T: Serialize + ?Sized>(&self, value: &T) -> TransportResult<Response> {
        let body = serde_json::to_vec(value)?;
        let mut refreshed = false;
        loop {
            let headers = self.request_headers("application/json, text/event-stream", None)?;
            let response = self
                .client
                .post(self.url.clone())
                .headers(headers)
                .header(CONTENT_TYPE, "application/json")
                .body(body.clone())
                .send()
                .await
                .map_err(|error| McpTransportError::Http {
                    status: 0,
                    body: error.to_string(),
                })?;

            let refresh = self.auth_refresh.read().clone();
            if !refreshed
                && matches!(
                    response.status(),
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                )
                && let Some(refresh) = refresh
            {
                let headers = refresh().await?;
                *self.headers.write() = headers;
                refreshed = true;
                continue;
            }
            self.capture_session_id(&response);
            return Ok(response);
        }
    }

    async fn validate_status(&self, response: Response) -> TransportResult<Response> {
        if response.status() == StatusCode::NOT_FOUND && self.session_id.read().is_some() {
            self.session_id.write().take();
            self.connected.store(false, Ordering::Release);
            if !self.closing.load(Ordering::Acquire) {
                self.callbacks.close();
            }
            return Err(McpTransportError::SessionExpired);
        }
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let auth_hint = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut body = response.text().await.unwrap_or_default();
        if let Some(hint) = auth_hint {
            body.push_str(&format!("; WWW-Authenticate: {hint}"));
        }
        Err(McpTransportError::Http { status, body })
    }

    async fn request_once(
        self: &Arc<Self>,
        id: RequestId,
        method: &str,
        params: Value,
    ) -> TransportResult<Value> {
        let request = JsonRpcRequest::new(id.clone(), method, params);
        let response = self
            .validate_status(self.send_json(&request).await?)
            .await?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();

        if content_type.starts_with("application/json") {
            let response: JsonRpcResponse = response
                .json()
                .await
                .map_err(|error| McpTransportError::Json(error.to_string()))?;
            if response.id.as_ref() != Some(&id) {
                return Err(McpTransportError::NoResponse {
                    request_id: format!("{id:?}"),
                });
            }
            return response.into_result().map_err(McpTransportError::from);
        }

        if content_type.starts_with("text/event-stream") {
            return self.consume_request_stream(response, id).await;
        }

        Err(McpTransportError::Http {
            status: response.status().as_u16(),
            body: format!("unexpected MCP response content type '{content_type}'"),
        })
    }

    async fn consume_request_stream(
        self: &Arc<Self>,
        mut response: Response,
        expected_id: RequestId,
    ) -> TransportResult<Value> {
        let mut decoder = SseDecoder::default();
        let mut last_event_id = None;
        let mut retry_ms = 0;
        let mut resume_count = 0usize;

        loop {
            while let Some(chunk) =
                response
                    .chunk()
                    .await
                    .map_err(|error| McpTransportError::Http {
                        status: 0,
                        body: error.to_string(),
                    })?
            {
                for event in decoder.push(&chunk).map_err(|error| {
                    McpTransportError::Json(format!("invalid UTF-8 in SSE stream: {error}"))
                })? {
                    if let Some(result) = self
                        .handle_sse_event(event, &expected_id, &mut last_event_id, &mut retry_ms)
                        .await?
                    {
                        return Ok(result);
                    }
                }
            }

            if let Some(event) = decoder.finish()
                && let Some(result) = self
                    .handle_sse_event(event, &expected_id, &mut last_event_id, &mut retry_ms)
                    .await?
            {
                return Ok(result);
            }

            let Some(event_id) = last_event_id.as_deref() else {
                return Err(McpTransportError::NoResponse {
                    request_id: format!("{expected_id:?}"),
                });
            };
            if resume_count >= MAX_SSE_RESUME_RETRIES {
                return Err(McpTransportError::NoResponse {
                    request_id: format!("{expected_id:?}"),
                });
            }
            resume_count += 1;
            if retry_ms > 0 {
                tokio::time::sleep(Duration::from_millis(retry_ms)).await;
            }
            response = self.open_get(Some(event_id)).await?;
            decoder = SseDecoder::default();
        }
    }

    async fn handle_sse_event(
        self: &Arc<Self>,
        event: SseEvent,
        expected_id: &RequestId,
        last_event_id: &mut Option<String>,
        retry_ms: &mut u64,
    ) -> TransportResult<Option<Value>> {
        if let Some(id) = event.id {
            *last_event_id = Some(id);
        }
        if let Some(retry) = event.retry_ms {
            *retry_ms = retry;
        }
        if event.data.is_empty() {
            return Ok(None);
        }

        match serde_json::from_str::<JsonRpcMessage>(&event.data)? {
            JsonRpcMessage::Response(response) => {
                if response.id.as_ref() == Some(expected_id) {
                    return response
                        .into_result()
                        .map(Some)
                        .map_err(McpTransportError::from);
                }
                tracing::debug!(?response.id, "ignoring unrelated MCP SSE response");
            }
            JsonRpcMessage::Notification(notification) => self.callbacks.notify(notification),
            JsonRpcMessage::Request(request) => {
                let inner = Arc::clone(self);
                tokio::spawn(async move {
                    inner.handle_server_request(request).await;
                });
            }
        }
        Ok(None)
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
        if let Err(error) = self.send_json(&response).await.and_then(|response| {
            if response.status().is_success() {
                Ok(response)
            } else {
                Err(McpTransportError::Http {
                    status: response.status().as_u16(),
                    body: "failed to post server request response".to_owned(),
                })
            }
        }) {
            tracing::warn!(%error, "failed to reply to MCP HTTP server request");
        }
    }

    async fn open_get(&self, last_event_id: Option<&str>) -> TransportResult<Response> {
        let headers = self.request_headers("text/event-stream", last_event_id)?;
        let response = self
            .client
            .get(self.url.clone())
            .headers(headers)
            .send()
            .await
            .map_err(|error| McpTransportError::Http {
                status: 0,
                body: error.to_string(),
            })?;
        self.capture_session_id(&response);
        self.validate_status(response).await
    }

    async fn listen(self: Arc<Self>, mut response: Response) {
        let mut decoder = SseDecoder::default();
        let mut last_event_id = None;
        let mut retry_ms = 0;
        loop {
            loop {
                let chunk = tokio::select! {
                    _ = self.cancellation.cancelled() => return,
                    result = response.chunk() => result,
                };
                match chunk {
                    Ok(Some(chunk)) => {
                        let events = match decoder.push(&chunk) {
                            Ok(events) => events,
                            Err(error) => {
                                tracing::warn!(%error, "invalid UTF-8 in MCP SSE listener");
                                return;
                            }
                        };
                        for event in events {
                            if let Err(error) = self
                                .handle_listener_event(event, &mut last_event_id, &mut retry_ms)
                                .await
                            {
                                tracing::warn!(%error, "invalid MCP SSE listener event");
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::debug!(%error, "MCP SSE listener ended with error");
                        break;
                    }
                }
            }

            if self.cancellation.is_cancelled() || !self.connected.load(Ordering::Acquire) {
                return;
            }
            if retry_ms > 0 {
                tokio::time::sleep(Duration::from_millis(retry_ms)).await;
            }
            match self.open_get(last_event_id.as_deref()).await {
                Ok(next) => {
                    response = next;
                    decoder = SseDecoder::default();
                }
                Err(error) => {
                    tracing::debug!(%error, "MCP SSE listener could not resume");
                    self.mark_closed();
                    return;
                }
            }
        }
    }

    async fn handle_listener_event(
        self: &Arc<Self>,
        event: SseEvent,
        last_event_id: &mut Option<String>,
        retry_ms: &mut u64,
    ) -> TransportResult<()> {
        if let Some(id) = event.id {
            *last_event_id = Some(id);
        }
        if let Some(retry) = event.retry_ms {
            *retry_ms = retry;
        }
        if event.data.is_empty() {
            return Ok(());
        }
        match serde_json::from_str::<JsonRpcMessage>(&event.data)? {
            JsonRpcMessage::Notification(notification) => self.callbacks.notify(notification),
            JsonRpcMessage::Request(request) => {
                let inner = Arc::clone(self);
                tokio::spawn(async move {
                    inner.handle_server_request(request).await;
                });
            }
            JsonRpcMessage::Response(response) => {
                tracing::debug!(?response.id, "ignoring response on background MCP SSE stream");
            }
        }
        Ok(())
    }
}

/// MCP Streamable HTTP transport.
pub struct HttpTransport {
    inner: Arc<HttpInner>,
}

impl HttpTransport {
    pub async fn connect(config: &McpServerConfig) -> TransportResult<Self> {
        let McpServerConfig::HttpOrSse { url, timeout, .. } = config else {
            return Err(McpTransportError::InvalidConfig(
                "expected HTTP server config".to_owned(),
            ));
        };
        if config.is_sse() {
            return Err(McpTransportError::InvalidConfig(
                "legacy SSE config cannot create HttpTransport".to_owned(),
            ));
        }
        config
            .validate()
            .map_err(|error| McpTransportError::InvalidConfig(error.to_string()))?;

        let url =
            Url::parse(url).map_err(|error| McpTransportError::InvalidConfig(error.to_string()))?;
        let inner = Arc::new(HttpInner {
            url,
            client: reqwest::Client::new(),
            headers: RwLock::new(headers_from_config(config)?),
            session_id: RwLock::new(None),
            protocol_version: RwLock::new(None),
            connected: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            timeout_ms: resolve_timeout_ms(
                std::env::var("OMP_MCP_TIMEOUT_MS").ok().as_deref(),
                *timeout,
            ),
            callbacks: TransportCallbacks::default(),
            cancellation: CancellationToken::new(),
            listener_task: Mutex::new(None),
            auth_refresh: RwLock::new(None),
        });
        Ok(Self { inner })
    }

    pub fn set_auth_refresh(&self, refresh: Option<AuthRefresh>) {
        *self.inner.auth_refresh.write() = refresh;
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn request(&self, method: &str, params: Value) -> TransportResult<Value> {
        if !self.is_connected() {
            return Err(McpTransportError::Disconnected);
        }
        let id = RequestId::from(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let request = self.inner.request_once(id, method, params);
        if self.inner.timeout_ms == 0 {
            request.await
        } else {
            tokio::time::timeout(Duration::from_millis(self.inner.timeout_ms), request)
                .await
                .map_err(|_| McpTransportError::Timeout {
                    method: method.to_owned(),
                    timeout_ms: self.inner.timeout_ms,
                })?
        }
    }

    async fn notify(&self, method: &str, params: Value) -> TransportResult<()> {
        if !self.is_connected() {
            return Err(McpTransportError::Disconnected);
        }
        let notification = JsonRpcNotification::new(method, params);
        let response = self.inner.send_json(&notification).await?;
        self.inner.validate_status(response).await?;
        Ok(())
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
        if let Some(handle) = self.inner.listener_task.lock().take() {
            handle.abort();
        }

        let session_id = self.inner.session_id.read().clone();
        if session_id.is_some() {
            let headers = self
                .inner
                .request_headers("application/json, text/event-stream", None)?;
            match self
                .inner
                .client
                .request(Method::DELETE, self.inner.url.clone())
                .headers(headers)
                .send()
                .await
            {
                Ok(response)
                    if response.status().is_success()
                        || response.status() == StatusCode::METHOD_NOT_ALLOWED => {}
                Ok(response) => {
                    tracing::debug!(
                        status = %response.status(),
                        "MCP HTTP session DELETE was not accepted"
                    );
                }
                Err(error) => tracing::debug!(%error, "MCP HTTP session DELETE failed"),
            }
            self.inner.session_id.write().take();
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

    async fn start_sse_listener(&self) -> TransportResult<()> {
        if self.inner.listener_task.lock().is_some() {
            return Ok(());
        }
        let open = self.inner.open_get(None);
        let response = if self.inner.timeout_ms == 0 {
            open.await
        } else {
            let deadline_ms = self.inner.timeout_ms.min(1_000);
            match tokio::time::timeout(Duration::from_millis(deadline_ms), open).await {
                Ok(response) => response,
                Err(_) => {
                    tracing::debug!(deadline_ms, "MCP background SSE listener startup timed out");
                    return Ok(());
                }
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(McpTransportError::Http { status: 405, .. }) => return Ok(()),
            Err(error) => {
                tracing::debug!(%error, "MCP server does not expose a background SSE listener");
                return Ok(());
            }
        };
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type
            .to_ascii_lowercase()
            .starts_with("text/event-stream")
        {
            return Ok(());
        }

        let inner = Arc::clone(&self.inner);
        let task = tokio::spawn(async move {
            inner.listen(response).await;
        });
        *self.inner.listener_task.lock() = Some(task);
        Ok(())
    }
}

impl Drop for HttpTransport {
    fn drop(&mut self) {
        self.inner.closing.store(true, Ordering::Release);
        self.inner.cancellation.cancel();
        if let Some(handle) = self.inner.listener_task.lock().take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn serve_once(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 16 * 1024];
            let size = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]).into_owned();
            socket.write_all(response.as_bytes()).await.unwrap();
            request
        });
        (format!("http://{address}/mcp"), task)
    }

    fn config(url: String) -> McpServerConfig {
        McpServerConfig::HttpOrSse {
            url,
            transport_type: None,
            headers: HashMap::new(),
            timeout: Some(5_000),
        }
    }

    #[tokio::test]
    async fn json_response_round_trip_and_required_headers() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nMCP-Session-Id: fixture\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let response: &'static str = Box::leak(response.into_boxed_str());
        let (url, server) = serve_once(response).await;
        let transport = HttpTransport::connect(&config(url)).await.unwrap();
        transport.set_protocol_version("2025-11-25");

        let result = transport.request("tools/list", json!({})).await.unwrap();
        assert_eq!(result, json!({"ok": true}));

        let request = server.await.unwrap().to_ascii_lowercase();
        assert!(request.contains("accept: application/json, text/event-stream"));
        assert!(request.contains("mcp-protocol-version: 2025-11-25"));
    }

    #[tokio::test]
    async fn sse_response_round_trip() {
        let sse = concat!(
            "id: event-1\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            sse.len(),
            sse
        );
        let response: &'static str = Box::leak(response.into_boxed_str());
        let (url, _server) = serve_once(response).await;
        let transport = HttpTransport::connect(&config(url)).await.unwrap();

        let result = transport.request("tools/list", json!({})).await.unwrap();
        assert_eq!(result, json!({"ok": true}));
    }

    #[tokio::test]
    async fn rejects_json_response_with_mismatched_request_id() {
        let body = r#"{"jsonrpc":"2.0","id":99,"result":{"ok":true}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let response: &'static str = Box::leak(response.into_boxed_str());
        let (url, _server) = serve_once(response).await;
        let transport = HttpTransport::connect(&config(url)).await.unwrap();

        let error = transport
            .request("tools/list", json!({}))
            .await
            .unwrap_err();

        assert!(matches!(error, McpTransportError::NoResponse { .. }));
    }

    #[test]
    fn configured_headers_are_validated() {
        let config = McpServerConfig::HttpOrSse {
            url: "https://example.com/mcp".to_owned(),
            transport_type: None,
            headers: HashMap::from([("bad header".to_owned(), "value".to_owned())]),
            timeout: None,
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let transport = runtime.block_on(HttpTransport::connect(&config)).unwrap();
        assert!(
            transport
                .inner
                .request_headers("application/json", None)
                .is_err()
        );
    }

    #[tokio::test]
    async fn http_listen_failure_triggers_close_callback() {
        // Create an HttpInner that will fail on open_get because the URL
        // points to a non-existent server.
        let inner = Arc::new(HttpInner {
            url: Url::parse("http://127.0.0.1:1").unwrap(),
            client: reqwest::Client::new(),
            headers: RwLock::new(HashMap::new()),
            session_id: RwLock::new(None),
            protocol_version: RwLock::new(None),
            connected: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            timeout_ms: 0,
            callbacks: TransportCallbacks::default(),
            cancellation: CancellationToken::new(),
            listener_task: Mutex::new(None),
            auth_refresh: RwLock::new(None),
        });

        let close_called = Arc::new(AtomicBool::new(false));
        let close_flag = close_called.clone();
        inner.callbacks.set_close(Arc::new(move || {
            close_flag.store(true, Ordering::Release);
        }));

        // Simulate a response that immediately ends, then open_get fails.
        // We call listen directly with a dummy response — but since we
        // can't easily create a Response, test the mark_closed path:
        inner.mark_closed();

        assert!(close_called.load(Ordering::Acquire));
        assert!(!inner.connected.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn http_listen_open_get_failure_notifies_close() {
        // Serve a single SSE response whose stream ends, forcing listen to
        // call open_get to resume. open_get points at an unreachable URL so
        // it fails, and listen must trigger mark_closed -> close callback.
        let sse = "retry: 0\n\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"params\":{}}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            sse.len(),
            sse
        );
        let response: &'static str = Box::leak(response.into_boxed_str());
        let (url, _server) = serve_once(response).await;

        // Build an HttpInner pointing at the served URL for the initial
        // response, then swap the URL to an unreachable address so open_get
        // fails on resume.
        let inner = Arc::new(HttpInner {
            url: Url::parse("http://127.0.0.1:1").unwrap(),
            client: reqwest::Client::new(),
            headers: RwLock::new(HashMap::new()),
            session_id: RwLock::new(None),
            protocol_version: RwLock::new(None),
            connected: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            timeout_ms: 0,
            callbacks: TransportCallbacks::default(),
            cancellation: CancellationToken::new(),
            listener_task: Mutex::new(None),
            auth_refresh: RwLock::new(None),
        });

        let close_called = Arc::new(AtomicBool::new(false));
        let close_flag = close_called.clone();
        inner.callbacks.set_close(Arc::new(move || {
            close_flag.store(true, Ordering::Release);
        }));

        // Fetch the initial response from the served URL, then run listen
        // with the unreachable inner URL so the resume open_get fails.
        let initial = reqwest::get(url).await.unwrap();
        // Drive the listen loop to completion.
        let inner_clone = Arc::clone(&inner);
        let handle = tokio::spawn(async move {
            inner_clone.listen(initial).await;
        });
        // Give the listener time to consume the stream and fail resuming.
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

        assert!(close_called.load(Ordering::Acquire), "close callback should fire on resume failure");
        assert!(!inner.connected.load(Ordering::Acquire), "connected should be false after resume failure");
    }
    #[test]
    fn sse_resume_limit_is_reasonable() {
        assert_eq!(MAX_SSE_RESUME_RETRIES, 10);
    }
}
