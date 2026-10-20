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
        return resp.header(header).cloned().ok_or_else(|| {
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
        // A repeated header (Set-Cookie) is an array; the first element the regex hits wins.
        let hays: Vec<String> = match rx.on.as_deref() {
            None | Some("body") => vec![resp.body.clone()],
            Some(other) => {
                let header = other.strip_prefix("header:").unwrap_or(other);
                match resp.header(header) {
                    Some(Value::Array(items)) => items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect(),
                    Some(v) => vec![v.as_str().unwrap_or("").to_string()],
                    None => vec![String::new()],
                }
            }
        };
        return hays
            .iter()
            .find_map(|hay| {
                re.captures(hay)
                    .and_then(|c| c.get(rx.group))
                    .map(|m| m.as_str().to_string())
            })
            .map(Value::String)
            .ok_or_else(|| CoreError::Harness(format!("capture `{name}`: regex matched nothing")));
    }
    Err(CoreError::Harness(format!(
        "capture `{name}`: no source configured"
    )))
}
