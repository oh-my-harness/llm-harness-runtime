mod http;
mod sse;
mod sse_codec;
mod stdio;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use parking_lot::RwLock;
use serde_json::Value;
use thiserror::Error;

use crate::config::McpServerConfig;
use crate::json_rpc::{
    JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RequestId, error_codes,
};

pub use http::{AuthRefresh, HttpTransport};
pub use sse::LegacySseTransport;
pub use stdio::StdioTransport;

pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, Error)]
pub enum McpTransportError {
    #[error("MCP transport is disconnected")]
    Disconnected,
    #[error("MCP transport closed")]
    Closed,
    #[error("MCP request '{method}' timed out after {timeout_ms}ms")]
    Timeout { method: String, timeout_ms: u64 },
    #[error("MCP I/O error: {0}")]
    Io(String),
    #[error("MCP JSON error: {0}")]
    Json(String),
    #[error("MCP HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("MCP session expired")]
    SessionExpired,
    #[error("MCP protocol error {code}: {message}")]
    Protocol {
        code: i32,
        message: String,
        data: Option<Value>,
    },
    #[error("MCP transport received no response for request {request_id}")]
    NoResponse { request_id: String },
    #[error("invalid MCP transport configuration: {0}")]
    InvalidConfig(String),
}

impl From<std::io::Error> for McpTransportError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

impl From<serde_json::Error> for McpTransportError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value.to_string())
    }
}

impl From<JsonRpcError> for McpTransportError {
    fn from(value: JsonRpcError) -> Self {
        Self::Protocol {
            code: value.code,
            message: value.message,
            data: value.data,
        }
    }
}

pub type TransportResult<T> = Result<T, McpTransportError>;
pub type RequestHandler =
    Arc<dyn Fn(JsonRpcRequest) -> BoxFuture<'static, TransportResult<Value>> + Send + Sync>;
pub type NotificationHandler = Arc<dyn Fn(JsonRpcNotification) + Send + Sync>;
pub type CloseHandler = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub(crate) struct TransportCallbacks {
    request: RwLock<Option<RequestHandler>>,
    notification: RwLock<Option<NotificationHandler>>,
    close: RwLock<Option<CloseHandler>>,
}

impl TransportCallbacks {
    pub(crate) fn set_request(&self, handler: RequestHandler) {
        *self.request.write() = Some(handler);
    }

    pub(crate) fn request(&self) -> Option<RequestHandler> {
        self.request.read().clone()
    }

    pub(crate) fn set_notification(&self, handler: NotificationHandler) {
        *self.notification.write() = Some(handler);
    }

    pub(crate) fn notify(&self, notification: JsonRpcNotification) {
        if let Some(handler) = self.notification.read().clone() {
            handler(notification);
        }
    }

    pub(crate) fn set_close(&self, handler: CloseHandler) {
        *self.close.write() = Some(handler);
    }

    pub(crate) fn clear_close(&self) {
        self.close.write().take();
    }

    pub(crate) fn close(&self) {
        if let Some(handler) = self.close.read().clone() {
            handler();
        }
    }
}

#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn request(&self, method: &str, params: Value) -> TransportResult<Value>;
    async fn notify(&self, method: &str, params: Value) -> TransportResult<()>;
    fn is_connected(&self) -> bool;
    async fn close(&self) -> TransportResult<()>;
    fn set_protocol_version(&self, version: &str);
    fn on_request(&self, handler: RequestHandler);
    fn on_notification(&self, handler: NotificationHandler);
    fn on_close(&self, handler: CloseHandler);
    fn clear_on_close(&self);

    async fn start_sse_listener(&self) -> TransportResult<()> {
        Ok(())
    }
}

pub async fn create_transport(config: &McpServerConfig) -> TransportResult<Arc<dyn McpTransport>> {
    match config {
        McpServerConfig::Stdio { .. } => Ok(Arc::new(StdioTransport::connect(config).await?)),
        McpServerConfig::HttpOrSse { .. } if config.is_sse() => {
            Ok(Arc::new(LegacySseTransport::connect(config).await?))
        }
        McpServerConfig::HttpOrSse { .. } => Ok(Arc::new(HttpTransport::connect(config).await?)),
    }
}

pub fn resolve_timeout_ms(env_override: Option<&str>, config_timeout: Option<u64>) -> u64 {
    if let Some(raw) = env_override {
        match raw.parse::<u64>() {
            Ok(value) => return value,
            Err(error) => {
                tracing::warn!(value = raw, %error, "ignoring invalid OMP_MCP_TIMEOUT_MS");
            }
        }
    }
    config_timeout.unwrap_or(DEFAULT_TIMEOUT_MS)
}

pub(crate) fn headers_from_config(
    config: &McpServerConfig,
) -> TransportResult<HashMap<String, String>> {
    match config {
        McpServerConfig::HttpOrSse { headers, .. } => Ok(headers.clone()),
        _ => Err(McpTransportError::InvalidConfig(
            "expected HTTP or SSE server config".to_owned(),
        )),
    }
}

pub(crate) fn request_error_response(id: RequestId, error: McpTransportError) -> JsonRpcResponse {
    let error = match error {
        McpTransportError::Protocol {
            code,
            message,
            data,
        } => JsonRpcError {
            code,
            message,
            data,
        },
        error => JsonRpcError {
            code: error_codes::INTERNAL_ERROR,
            message: error.to_string(),
            data: None,
        },
    };
    JsonRpcResponse::error(Some(id), error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_resolution_obeys_precedence_and_zero() {
        assert_eq!(resolve_timeout_ms(Some("10"), Some(20)), 10);
        assert_eq!(resolve_timeout_ms(Some("0"), Some(20)), 0);
        assert_eq!(resolve_timeout_ms(None, Some(20)), 20);
        assert_eq!(resolve_timeout_ms(None, None), DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn invalid_timeout_override_falls_back_to_config() {
        assert_eq!(resolve_timeout_ms(Some("invalid"), Some(20)), 20);
    }

    #[test]
    fn server_request_protocol_errors_keep_their_json_rpc_code() {
        let response = request_error_response(
            RequestId::from(1_u64),
            McpTransportError::Protocol {
                code: error_codes::METHOD_NOT_FOUND,
                message: "missing".to_owned(),
                data: None,
            },
        );

        assert_eq!(response.error.unwrap().code, error_codes::METHOD_NOT_FOUND);
    }
}
