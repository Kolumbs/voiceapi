//! Wire protocol: the closed set of client requests, the structured error type,
//! and helpers to parse requests and build JSON responses.
//!
//! The API is deliberately small and closed — clients cannot issue raw AT
//! commands. Unknown ops or malformed requests are rejected as `bad_request`.

use serde::Deserialize;
use serde_json::{json, Map, Value};

/// A structured, client-facing error with a stable machine-readable `code`.
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
    pub fn modem(message: impl Into<String>) -> Self {
        Self::new("modem_error", message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new("bad_request", message)
    }
    pub fn modem_not_ready() -> Self {
        Self::new("modem_not_ready", "modem is not ready")
    }
    pub fn storage(message: impl Into<String>) -> Self {
        Self::new("storage_error", message)
    }
    pub fn already_connected() -> Self {
        Self::new("already_connected", "another client is already connected")
    }
}

/// Default and ceiling for `list_errors`, so a client cannot ask the service to
/// serialize an unbounded table.
const DEFAULT_ERROR_LIMIT: u32 = 20;
pub const MAX_ERROR_LIMIT: u32 = 200;

fn default_error_limit() -> u32 {
    DEFAULT_ERROR_LIMIT
}

/// The closed set of operations. `op` is the discriminant; unknown values fail
/// to deserialize and are reported as `bad_request`.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RequestBody {
    ListSms,
    ReadSms { index: u32 },
    DeleteSms { index: u32 },
    Health,
    ListErrors {
        #[serde(default = "default_error_limit")]
        limit: u32,
    },
}

impl RequestBody {
    /// Stable name used for the audit log's `op` column.
    pub fn op_name(&self) -> &'static str {
        match self {
            RequestBody::ListSms => "list_sms",
            RequestBody::ReadSms { .. } => "read_sms",
            RequestBody::DeleteSms { .. } => "delete_sms",
            RequestBody::Health => "health",
            RequestBody::ListErrors { .. } => "list_errors",
        }
    }
}

/// Parse a client message. On success returns the (optional) correlation id and
/// the typed request. On failure returns the id (best-effort, so the error can
/// still be correlated) plus a `bad_request` error.
pub fn parse_request(text: &str) -> Result<(Option<i64>, RequestBody), (Option<i64>, ApiError)> {
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return Err((None, ApiError::bad_request(format!("invalid JSON: {e}")))),
    };
    let id = value.get("id").and_then(Value::as_i64);
    match serde_json::from_value::<RequestBody>(value) {
        Ok(body) => Ok((id, body)),
        Err(e) => Err((id, ApiError::bad_request(e.to_string()))),
    }
}

/// Build a success response: `{ "id": <id>, "ok": true, ...payload }`.
/// `payload` must be a JSON object; its fields are merged into the envelope.
pub fn ok_json(id: Option<i64>, payload: Value) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), json!(id));
    map.insert("ok".into(), json!(true));
    if let Value::Object(o) = payload {
        for (k, v) in o {
            map.insert(k, v);
        }
    }
    Value::Object(map)
}

/// Build an error response: `{ "id": <id>, "ok": false, "error": {code, message} }`.
pub fn error_json(id: Option<i64>, e: &ApiError) -> Value {
    json!({
        "id": id,
        "ok": false,
        "error": { "code": e.code, "message": e.message },
    })
}
