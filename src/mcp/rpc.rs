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

/// A request id MCP accepts: a string or an integer (never null or a float).
pub fn is_valid_id(id: &Value) -> bool {
    id.is_string() || id.is_i64() || id.is_u64()
}

/// JSON-RPC 2.0 shape of a raw message: a `jsonrpc: "2.0"` object whose
/// `id`, when the key is present, is a valid id, and whose `params`, when
/// present, is an object. Checked before deserializing so an explicit
/// `null` is rejected instead of being read as an absent field.
pub fn validate_shape(value: &Value) -> Result<(), String> {
    let Some(obj) = value.as_object() else {
        return Err("Invalid Request: expected a JSON object".into());
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err("Invalid Request: jsonrpc must be \"2.0\"".into());
    }
    if let Some(id) = obj.get("id") {
        if !is_valid_id(id) {
            return Err("Invalid Request: id must be a string or an integer".into());
        }
    }
    if obj.get("method").and_then(Value::as_str).is_none() {
        return Err("Invalid Request: method must be a string".into());
    }
    if let Some(params) = obj.get("params") {
        if !params.is_object() {
            return Err("Invalid Request: params must be an object".into());
        }
    }
    Ok(())
}

impl Request {
    /// Whether a modern request carries the required client capabilities.
    pub fn has_meta_client_capabilities(&self) -> bool {
        self.params
            .get("_meta")
            .and_then(|m| m.get("io.modelcontextprotocol/clientCapabilities"))
            .is_some_and(Value::is_object)
    }

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

    /// Whether `_meta` carries the per-request envelope keys of 2026-07-28
    /// (version, identity, capabilities, log level). Other reserved keys,
    /// such as `related-task` from 2025-11-25, do not mark a modern client.
    pub fn has_modern_meta(&self) -> bool {
        const MODERN_KEYS: [&str; 4] = [
            "io.modelcontextprotocol/protocolVersion",
            "io.modelcontextprotocol/clientInfo",
            "io.modelcontextprotocol/clientCapabilities",
            "io.modelcontextprotocol/logLevel",
        ];
        self.params
            .get("_meta")
            .and_then(Value::as_object)
            .is_some_and(|m| MODERN_KEYS.iter().any(|k| m.contains_key(*k)))
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
