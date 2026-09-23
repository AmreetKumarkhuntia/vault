use serde_json::{json, Value};
use serde_json_path::JsonPath;
use vault_dsl::CaptureSpec;

use crate::http::StepResponse;
use crate::CoreError;

pub fn capture_value(
    name: &str,
    spec: &CaptureSpec,
    resp: &StepResponse,
) -> Result<Value, CoreError> {
    if let Some(path) = &spec.jsonpath {
        let compiled = JsonPath::parse(path)
            .map_err(|e| CoreError::Harness(format!("capture `{name}`: bad jsonpath: {e}")))?;
        return compiled
            .query(&resp.body_json)
            .all()
            .first()
            .map(|v| (*v).clone())
            .ok_or_else(|| {
                CoreError::Harness(format!("capture `{name}`: `{path}` matched nothing"))
            });
    }
    if let Some(header) = &spec.header {
        return resp
            .headers
            .as_object()
            .and_then(|h| h.iter().find(|(k, _)| k.eq_ignore_ascii_case(header)))
            .map(|(_, v)| v.clone())
            .ok_or_else(|| {
                CoreError::Harness(format!("capture `{name}`: header `{header}` absent"))
            });
    }
    if spec.status == Some(true) {
        return Ok(json!(resp.status));
    }
    if spec.body.is_some() {
        return Ok(Value::String(resp.body.clone()));
    }
    if let Some(rx) = &spec.regex {
        let re = regex::Regex::new(&rx.pattern)
            .map_err(|e| CoreError::Harness(format!("capture `{name}`: bad regex: {e}")))?;
        let hay = match rx.on.as_deref() {
            None | Some("body") => resp.body.clone(),
            Some(other) => {
                let header = other.strip_prefix("header:").unwrap_or(other);
                resp.headers
                    .as_object()
                    .and_then(|h| h.iter().find(|(k, _)| k.eq_ignore_ascii_case(header)))
                    .and_then(|(_, v)| v.as_str())
                    .unwrap_or("")
                    .to_string()
            }
        };
        return re
            .captures(&hay)
            .and_then(|c| c.get(rx.group))
            .map(|m| Value::String(m.as_str().to_string()))
            .ok_or_else(|| CoreError::Harness(format!("capture `{name}`: regex matched nothing")));
    }
    Err(CoreError::Harness(format!(
        "capture `{name}`: no source configured"
    )))
}
