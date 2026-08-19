use std::collections::HashSet;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thiserror::Error;

use crate::json_rpc::JsonRpcRequest;
use crate::transport::{
    CloseHandler, McpTransport, McpTransportError, NotificationHandler, TransportResult,
};
use crate::types::{
    McpGetPromptResult, McpImplementation, McpInitializeResult, McpPrompt, McpPromptArguments,
    McpResource, McpResourceReadResult, McpServerCapabilities, McpToolCallResult,
    McpToolDefinition,
};

pub const PREFERRED_PROTOCOL_VERSION: &str = "2025-11-25";

/// Protocol revisions accepted during `initialize` negotiation, ordered by
/// preference. The preferred version is listed first; legacy revisions follow
/// so that servers responding with older versions (notably `2024-11-05` for
/// the legacy SSE transport) are not rejected.
///
/// Compatibility testing for the non-preferred revisions is pending — they are
/// accepted based on wire-shape parity with the preferred version per the MCP
/// versioning contract. Do not add further revisions without compatibility
/// tests covering handshake, tools/list, tools/call, resources, and prompts.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    PREFERRED_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];
const MAX_PAGINATION_PAGES: usize = 100;

#[derive(Debug, Error)]
pub enum McpSessionError {
    #[error(transparent)]
    Transport(#[from] McpTransportError),
    #[error("invalid MCP response: {0}")]
    InvalidResponse(String),
    #[error("unsupported MCP protocol version '{0}'")]
    UnsupportedProtocolVersion(String),
    #[error("MCP server does not support {0}")]
    MethodNotSupported(&'static str),
    #[error("MCP {method} exceeded the {max_pages}-page safety limit")]
    PaginationLimit {
        method: &'static str,
        max_pages: usize,
    },
    #[error("MCP {method} repeated pagination cursor '{cursor}'")]
    RepeatedCursor {
        method: &'static str,
        cursor: String,
    },
}

pub type SessionResult<T> = Result<T, McpSessionError>;

/// An initialized MCP protocol session over a concrete transport.
pub struct McpSession {
    transport: Arc<dyn McpTransport>,
    capabilities: RwLock<McpServerCapabilities>,
    server_info: RwLock<McpImplementation>,
    protocol_version: RwLock<String>,
}

impl McpSession {
    /// Complete the MCP client handshake over an already-connected transport.
    pub async fn connect(transport: Arc<dyn McpTransport>) -> SessionResult<Self> {
        transport.on_request(Arc::new(default_server_request));

        let result = Self::initialize(Arc::clone(&transport)).await;
        if result.is_err() {
            transport.clear_on_close();
            if let Err(close_error) = transport.close().await {
                tracing::debug!(%close_error, "failed to close MCP transport after handshake error");
            }
        }
        result
    }

    async fn initialize(transport: Arc<dyn McpTransport>) -> SessionResult<Self> {
        let value = transport
            .request(
                "initialize",
                json!({
                    "protocolVersion": PREFERRED_PROTOCOL_VERSION,
                    "capabilities": {
                        "roots": {"listChanged": false}
                    },
                    "clientInfo": {
                        "name": "llm-harness-mcp",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;
        let init: McpInitializeResult = serde_json::from_value(value)
            .map_err(|error| McpSessionError::InvalidResponse(error.to_string()))?;

        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&init.protocol_version.as_str()) {
            return Err(McpSessionError::UnsupportedProtocolVersion(
                init.protocol_version,
            ));
        }

        transport.set_protocol_version(&init.protocol_version);
        transport.start_sse_listener().await?;
        transport
            .notify("notifications/initialized", json!({}))
            .await?;

        Ok(Self {
            transport,
            capabilities: RwLock::new(init.capabilities),
            server_info: RwLock::new(init.server_info),
            protocol_version: RwLock::new(init.protocol_version),
        })
    }

    pub async fn list_tools(&self) -> SessionResult<Vec<McpToolDefinition>> {
        self.require_capability("tools", |capabilities| capabilities.tools.is_some())?;
        self.paginate("tools/list", "tools").await
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> SessionResult<McpToolCallResult> {
        self.require_capability("tools", |capabilities| capabilities.tools.is_some())?;
        self.request_typed("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    pub async fn list_resources(&self) -> SessionResult<Vec<McpResource>> {
        self.require_capability("resources", |capabilities| capabilities.resources.is_some())?;
        self.paginate("resources/list", "resources").await
    }

    pub async fn read_resource(&self, uri: &str) -> SessionResult<McpResourceReadResult> {
        self.require_capability("resources", |capabilities| capabilities.resources.is_some())?;
        self.request_typed("resources/read", json!({"uri": uri}))
            .await
    }

    pub async fn list_prompts(&self) -> SessionResult<Vec<McpPrompt>> {
        self.require_capability("prompts", |capabilities| capabilities.prompts.is_some())?;
        self.paginate("prompts/list", "prompts").await
    }

    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: McpPromptArguments,
    ) -> SessionResult<McpGetPromptResult> {
        self.require_capability("prompts", |capabilities| capabilities.prompts.is_some())?;
        self.request_typed("prompts/get", json!({"name": name, "arguments": arguments}))
            .await
    }

    pub async fn disconnect(&self) -> SessionResult<()> {
        self.transport.clear_on_close();
        self.transport.close().await?;
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.transport.is_connected()
    }

    pub fn capabilities(&self) -> McpServerCapabilities {
        self.capabilities.read().clone()
    }

    pub fn server_info(&self) -> McpImplementation {
        self.server_info.read().clone()
    }

    pub fn protocol_version(&self) -> String {
        self.protocol_version.read().clone()
    }

    pub fn on_notification(&self, handler: NotificationHandler) {
        self.transport.on_notification(handler);
    }

    pub fn on_close(&self, handler: CloseHandler) {
        self.transport.on_close(handler);
    }

    pub fn clear_on_close(&self) {
        self.transport.clear_on_close();
    }

    async fn request_typed<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> SessionResult<T> {
        let value = self.transport.request(method, params).await?;
        serde_json::from_value(value)
            .map_err(|error| McpSessionError::InvalidResponse(error.to_string()))
    }

    async fn paginate<T: DeserializeOwned>(
        &self,
        method: &'static str,
        items_field: &'static str,
    ) -> SessionResult<Vec<T>> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();

        for _ in 0..MAX_PAGINATION_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
            let value = self.transport.request(method, params).await?;
            let mut object = value.as_object().cloned().ok_or_else(|| {
                McpSessionError::InvalidResponse(format!("{method} result must be an object"))
            })?;
            let page_items = object.remove(items_field).unwrap_or_else(|| json!([]));
            let page_items: Vec<T> = serde_json::from_value(page_items)
                .map_err(|error| McpSessionError::InvalidResponse(error.to_string()))?;
            items.extend(page_items);

            let next = match object.remove("nextCursor") {
                None | Some(Value::Null) => return Ok(items),
                Some(Value::String(next)) if !next.is_empty() => next,
                Some(Value::String(_)) => return Ok(items),
                Some(_) => {
                    return Err(McpSessionError::InvalidResponse(format!(
                        "{method} nextCursor must be a string"
                    )));
                }
            };
            if !seen.insert(next.clone()) {
                return Err(McpSessionError::RepeatedCursor {
                    method,
                    cursor: next,
                });
            }
            cursor = Some(next);
        }

        Err(McpSessionError::PaginationLimit {
            method,
            max_pages: MAX_PAGINATION_PAGES,
        })
    }

    fn require_capability(
        &self,
        name: &'static str,
        predicate: impl FnOnce(&McpServerCapabilities) -> bool,
    ) -> SessionResult<()> {
        if predicate(&self.capabilities.read()) {
            Ok(())
        } else {
            Err(McpSessionError::MethodNotSupported(name))
        }
    }
}

fn default_server_request(
    request: JsonRpcRequest,
) -> futures::future::BoxFuture<'static, TransportResult<Value>> {
    Box::pin(async move {
        match request.method.as_str() {
            "roots/list" => Ok(json!({"roots": []})),
            "ping" => Ok(json!({})),
            method => Err(McpTransportError::Protocol {
                code: crate::json_rpc::error_codes::METHOD_NOT_FOUND,
                message: format!("Method not found: {method}"),
                data: None,
            }),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use parking_lot::Mutex;

    use super::*;
    use crate::transport::{RequestHandler, TransportResult};

    struct MockTransport {
        connected: AtomicBool,
        responses: Mutex<VecDeque<TransportResult<Value>>>,
        calls: Mutex<Vec<(String, Value)>>,
        notifications: Mutex<Vec<(String, Value)>>,
        protocol_version: Mutex<Option<String>>,
        request_handler: Mutex<Option<RequestHandler>>,
        listener_started: AtomicBool,
    }

    impl MockTransport {
        fn new(responses: impl IntoIterator<Item = Value>) -> Self {
            Self {
                connected: AtomicBool::new(true),
                responses: Mutex::new(responses.into_iter().map(Ok).collect()),
                calls: Mutex::new(Vec::new()),
                notifications: Mutex::new(Vec::new()),
                protocol_version: Mutex::new(None),
                request_handler: Mutex::new(None),
                listener_started: AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl McpTransport for MockTransport {
        async fn request(&self, method: &str, params: Value) -> TransportResult<Value> {
            self.calls.lock().push((method.to_owned(), params));
            self.responses
                .lock()
                .pop_front()
                .unwrap_or(Err(McpTransportError::NoResponse {
                    request_id: method.to_owned(),
                }))
        }

        async fn notify(&self, method: &str, params: Value) -> TransportResult<()> {
            self.notifications.lock().push((method.to_owned(), params));
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::Acquire)
        }

        async fn close(&self) -> TransportResult<()> {
            self.connected.store(false, Ordering::Release);
            Ok(())
        }

        fn set_protocol_version(&self, version: &str) {
            *self.protocol_version.lock() = Some(version.to_owned());
        }

        fn on_request(&self, handler: RequestHandler) {
            *self.request_handler.lock() = Some(handler);
        }

        fn on_notification(&self, _handler: NotificationHandler) {}

        fn on_close(&self, _handler: CloseHandler) {}

        fn clear_on_close(&self) {}

        async fn start_sse_listener(&self) -> TransportResult<()> {
            self.listener_started.store(true, Ordering::Release);
            Ok(())
        }
    }

    fn initialize_result(capabilities: Value) -> Value {
        json!({
            "protocolVersion": PREFERRED_PROTOCOL_VERSION,
            "capabilities": capabilities,
            "serverInfo": {"name": "fixture", "version": "1.0.0"}
        })
    }

    #[tokio::test]
    async fn handshake_negotiates_version_before_initialized_notification() {
        let transport = Arc::new(MockTransport::new([initialize_result(json!({
            "tools": {}
        }))]));
        let session = McpSession::connect(transport.clone()).await.unwrap();

        assert_eq!(session.protocol_version(), PREFERRED_PROTOCOL_VERSION);
        assert_eq!(
            transport.protocol_version.lock().as_deref(),
            Some(PREFERRED_PROTOCOL_VERSION)
        );
        assert!(transport.listener_started.load(Ordering::Acquire));
        assert_eq!(
            transport.notifications.lock().as_slice(),
            &[("notifications/initialized".to_owned(), json!({}))]
        );
        let calls = transport.calls.lock();
        assert_eq!(calls[0].0, "initialize");
        assert_eq!(
            calls[0].1["protocolVersion"],
            json!(PREFERRED_PROTOCOL_VERSION)
        );
    }

    #[tokio::test]
    async fn rejects_untested_protocol_version_and_closes_transport() {
        let transport = Arc::new(MockTransport::new([json!({
            "protocolVersion": "2023-12-31",
            "capabilities": {},
            "serverInfo": {"name": "fixture", "version": "1"}
        })]));

        let error = match McpSession::connect(transport.clone()).await {
            Ok(_) => panic!("untested protocol version was accepted"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            McpSessionError::UnsupportedProtocolVersion(_)
        ));
        assert!(!transport.is_connected());
    }
    #[test]
    fn supported_protocol_versions_includes_legacy() {
        assert!(
            SUPPORTED_PROTOCOL_VERSIONS.contains(&"2025-11-25"),
            "must include preferred version"
        );
        assert!(
            SUPPORTED_PROTOCOL_VERSIONS.contains(&"2025-06-18"),
            "must include 2025-06-18"
        );
        assert!(
            SUPPORTED_PROTOCOL_VERSIONS.contains(&"2025-03-26"),
            "must include 2025-03-26"
        );
        assert!(
            SUPPORTED_PROTOCOL_VERSIONS.contains(&"2024-11-05"),
            "must include legacy SSE version"
        );
    }

    #[tokio::test]
    async fn default_handler_supports_roots_and_ping() {
        let transport = Arc::new(MockTransport::new([initialize_result(json!({}))]));
        let _session = McpSession::connect(transport.clone()).await.unwrap();
        let handler = transport.request_handler.lock().clone().unwrap();

        let roots = handler(JsonRpcRequest::new(1_u64, "roots/list", json!({})))
            .await
            .unwrap();
        let ping = handler(JsonRpcRequest::new(2_u64, "ping", json!({})))
            .await
            .unwrap();

        assert_eq!(roots, json!({"roots": []}));
        assert_eq!(ping, json!({}));
    }

    #[tokio::test]
    async fn capability_check_prevents_unsupported_network_call() {
        let transport = Arc::new(MockTransport::new([initialize_result(json!({}))]));
        let session = McpSession::connect(transport.clone()).await.unwrap();
        let error = session.list_tools().await.unwrap_err();

        assert!(matches!(
            error,
            McpSessionError::MethodNotSupported("tools")
        ));
        assert_eq!(transport.calls.lock().len(), 1);
    }

    #[tokio::test]
    async fn paginates_tools_until_cursor_is_absent() {
        let transport = Arc::new(MockTransport::new([
            initialize_result(json!({"tools": {}})),
            json!({
                "tools": [{
                    "name": "one",
                    "description": "",
                    "inputSchema": {"type": "object"}
                }],
                "nextCursor": "page-2"
            }),
            json!({
                "tools": [{
                    "name": "two",
                    "description": "",
                    "inputSchema": {"type": "object"}
                }]
            }),
        ]));
        let session = McpSession::connect(transport.clone()).await.unwrap();

        let tools = session.list_tools().await.unwrap();

        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!(transport.calls.lock()[2].1, json!({"cursor": "page-2"}));
    }

    #[tokio::test]
    async fn detects_repeated_pagination_cursor() {
        let transport = Arc::new(MockTransport::new([
            initialize_result(json!({"resources": {}})),
            json!({"resources": [], "nextCursor": "same"}),
            json!({"resources": [], "nextCursor": "same"}),
        ]));
        let session = McpSession::connect(transport).await.unwrap();

        let error = session.list_resources().await.unwrap_err();

        assert!(matches!(error, McpSessionError::RepeatedCursor { .. }));
    }
}
