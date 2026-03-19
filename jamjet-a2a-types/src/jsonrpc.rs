//! JSON-RPC 2.0 envelope types for A2A v1.0.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ────────────────────────────────────────────────────────────────────────────
// JsonRpcRequest
// ────────────────────────────────────────────────────────────────────────────

/// A JSON-RPC 2.0 request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest<P> {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    pub params: P,
}

impl<P: Serialize> JsonRpcRequest<P> {
    /// Create a new JSON-RPC 2.0 request with a default id of `1`.
    pub fn new(method: impl Into<String>, params: P) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Value::Number(1.into()),
            method: method.into(),
            params,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// JsonRpcResponse
// ────────────────────────────────────────────────────────────────────────────

/// A JSON-RPC 2.0 response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse<R> {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<R>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

// ────────────────────────────────────────────────────────────────────────────
// JsonRpcError
// ────────────────────────────────────────────────────────────────────────────

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_has_version_2() {
        let req = JsonRpcRequest::new("SendMessage", serde_json::json!({}));
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["method"], "SendMessage");
    }
}
