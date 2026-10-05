use serde::Serialize;
use serde_json::{json, Value};

mod bundle;

/// A portable report document. All evidence must be sanitized by the caller.
/// The HTML renderer only bounds previews and escapes data for embedding.
#[derive(Debug, Serialize)]
pub struct HtmlReport {
    pub schema_version: u32,
    pub run: Value,
    pub metadata: Value,
    pub executions: Vec<Value>,
}

const PREVIEW_LIMIT: usize = 64 * 1024;
const STYLE: &str = include_str!("html/style.css");

/// Render a standalone document with embedded styles for in-memory callers.
/// Use `write_html` to generate a bundle with a shared external stylesheet.
pub fn to_html(report: &HtmlReport) -> String {
    render(report, &format!("<style>{STYLE}</style>"))
}

fn render(report: &HtmlReport, style: &str) -> String {
    let mut data = serde_json::to_value(report).expect("JSON report values serialize");
    if let Some(tests) = data.pointer_mut("/run/tests").and_then(Value::as_array_mut) {
        for test in tests {
            bound_evidence(test);
        }
    }
    if let Some(tests) = data
        .pointer_mut("/metadata/tests")
        .and_then(Value::as_array_mut)
    {
        for test in tests {
            if let Some(inputs) = test.get_mut("with") {
                bound_payload(inputs);
            }
        }
    }
    if let Some(executions) = data.get_mut("executions").and_then(Value::as_array_mut) {
        for execution in executions {
            if let Some(events) = execution.get_mut("events").and_then(Value::as_array_mut) {
                for event in events {
                    let is_capture = event.get("phase").and_then(Value::as_str) == Some("capture");
                    if let Some(details) = event.get_mut("details") {
                        if is_capture {
                            if let Some(value) = details.get_mut("value") {
                                bound_payload(value);
                            }
                        }
                        bound_evidence(details);
                    }
                }
            }
            if let Some(exports) = execution.get_mut("exports") {
                bound_payload(exports);
                bound_evidence(exports);
            }
        }
    }
    let data = serde_json::to_string(&data)
        .expect("JSON report values serialize")
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    let document = include_str!("html/document.html");
    document
        .replace("__VAULT_STYLE__", style)
        .replace("__VAULT_SCRIPT__", include_str!("html/report.js"))
        .replace("__VAULT_DATA__", &data)
}

pub fn write_html(report: &HtmlReport, path: &str) -> std::io::Result<()> {
    bundle::write(report, std::path::Path::new(path))
}

fn bound_evidence(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if matches!(
                    key.as_str(),
                    "body"
                        | "body_json"
                        | "json"
                        | "headers"
                        | "query"
                        | "form"
                        | "captures"
                        | "expected"
                        | "actual"
                        | "before"
                        | "after"
                        | "interleaving"
                        | "document"
                        | "seed_doc"
                ) && bound_payload(value)
                {
                    continue;
                }
                bound_evidence(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                bound_evidence(value);
            }
        }
        _ => {}
    }
}

fn bound_payload(value: &mut Value) -> bool {
    let text = match &*value {
        Value::String(text) => text.clone(),
        value => serde_json::to_string_pretty(value).expect("JSON evidence serializes"),
    };
    if text.len() <= PREVIEW_LIMIT {
        return false;
    }
    let original_bytes = text.len();
    let original_entries = match value {
        Value::Array(values) => Some(values.len()),
        Value::Object(values) => Some(values.len()),
        _ => None,
    };
    let mut end = PREVIEW_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    *value = json!({
        "_vault_preview": true,
        "preview": &text[..end],
        "original_bytes": original_bytes,
        "omitted_bytes": original_bytes - end,
        "original_entries": original_entries,
    });
    true
}
