use serde::Serialize;
use serde_json::Value;

/// One request the mock received — recorded whether or not a stub matched.
#[derive(Debug, Clone, Serialize)]
pub struct RecordedExchange {
    /// Global monotonic order, for `ordered:` groups.
    pub seq: u64,
    pub at_unix_ms: i64,
    /// Dependency name resolved from the path prefix ("payments"), or
    /// "<unknown>" when no prefix matched.
    pub dependency: String,
    pub request: RecordedRequest,
    /// Name of the stub that served it; None = unmatched.
    pub matched_stub: Option<String>,
    pub responded_status: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordedRequest {
    pub method: String,
    /// Path relative to the dependency prefix ("/v1/charges").
    pub path: String,
    pub query: Value,
    pub headers: Value,
    /// Raw body as UTF-8 (lossy).
    pub body: String,
    /// Parsed JSON body when the raw body parses; Null otherwise.
    pub body_json: Value,
}

impl RecordedExchange {
    pub fn to_report_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}
