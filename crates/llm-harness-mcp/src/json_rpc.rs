use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

/// An MCP JSON-RPC request identifier.
///
/// MCP accepts both string and numeric identifiers. This client emits numeric
/// identifiers, but accepts either form from servers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    Number(Number),
}

impl From<u64> for RequestId {
    fn from(value: u64) -> Self {
        Self::Number(Number::from(value))
    }
}

impl From<String> for RequestId {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

/// A JSON-RPC 2.0 request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl JsonRpcRequest {
    pub fn new(id: impl Into<RequestId>, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC 2.0 notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A JSON-RPC 2.0 response.
///
/// `id` may be null for an error that could not be associated with a request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<RequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn success(id: impl Into<RequestId>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id: Some(id.into()),
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<RequestId>, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: None,
            error: Some(error),
        }
    }

    pub fn into_result(self) -> Result<Value, JsonRpcError> {
        match (self.result, self.error) {
            (_, Some(error)) => Err(error),
            (Some(result), None) => Ok(result),
            (None, None) => Err(JsonRpcError {
                code: -32600,
                message: "Invalid response: missing both result and error".to_owned(),
                data: None,
            }),
        }
    }
}

/// Any JSON-RPC message shape used by MCP.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Response(JsonRpcResponse),
}

pub mod error_codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_id_accepts_string_and_number() {
        let string_id: RequestId = serde_json::from_value(json!("request-1")).unwrap();
        let number_id: RequestId = serde_json::from_value(json!(42)).unwrap();

        assert_eq!(string_id, RequestId::String("request-1".to_owned()));
        assert_eq!(number_id, RequestId::from(42_u64));
    }

    #[test]
    fn message_discriminates_request_notification_and_response() {
        let request: JsonRpcMessage = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        }))
        .unwrap();
        let notification: JsonRpcMessage = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .unwrap();
        let response: JsonRpcMessage = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": "abc",
            "result": {"tools": []}
        }))
        .unwrap();

        assert!(matches!(request, JsonRpcMessage::Request(_)));
        assert!(matches!(notification, JsonRpcMessage::Notification(_)));
        assert!(matches!(response, JsonRpcMessage::Response(_)));
    }

    #[test]
    fn response_with_null_id_and_error_deserializes() {
        let response: JsonRpcResponse = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {
                "code": -32700,
                "message": "parse error"
            }
        }))
        .unwrap();

        assert_eq!(response.id, None);
        assert_eq!(response.error.unwrap().code, error_codes::PARSE_ERROR);
    }

    #[test]
    fn constructors_emit_json_rpc_2() {
        let request = JsonRpcRequest::new(7_u64, "ping", Value::Null);
        let notification = JsonRpcNotification::new("notifications/initialized", json!({}));
        let response = JsonRpcResponse::success(7_u64, json!({}));

        assert_eq!(request.jsonrpc, "2.0");
        assert_eq!(notification.jsonrpc, "2.0");
        assert_eq!(response.jsonrpc, "2.0");
    }
    #[test]
    fn into_result_rejects_missing_result_and_error() {
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_owned(),
            id: Some(RequestId::from(1u64)),
            result: None,
            error: None,
        };
        let result = response.into_result();
        assert!(result.is_err(), "missing result and error must be rejected");
        let err = result.unwrap_err();
        assert_eq!(err.code, -32600);
        assert!(err.message.contains("missing"));
    }
}
