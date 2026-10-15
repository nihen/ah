//! JSON-RPC 2.0 message shapes and error codes used by the MCP server.
use serde::Deserialize;
use serde_json::{Map, Value, json};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
/// MCP 2026-07-28 `UnsupportedProtocolVersionError`.
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// An incoming request or notification (no `id`).
#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn params_map(&self) -> Option<&Map<String, Value>> {
        self.params.as_object()
    }

    /// `_meta.io.modelcontextprotocol/protocolVersion` of a modern request.
    pub fn meta_protocol_version(&self) -> Option<&str> {
        self.params
            .get("_meta")?
            .get("io.modelcontextprotocol/protocolVersion")?
            .as_str()
    }
}

/// A protocol-level error (as opposed to a tool execution error, which is
/// reported inside a successful `tools/call` result).
#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }
}

pub fn response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error(id: &Value, err: &RpcError) -> Value {
    let mut e = Map::new();
    e.insert("code".into(), err.code.into());
    e.insert("message".into(), err.message.clone().into());
    if let Some(data) = &err.data {
        e.insert("data".into(), data.clone());
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": Value::Object(e) })
}
