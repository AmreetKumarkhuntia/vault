use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use vault_dsl::RequestSpec;

use crate::execution::ExecutionCollector;
use crate::{CoreError, TemplateEngine};

#[derive(Debug, Clone)]
pub struct StepResponse {
    pub status: u16,
    pub headers: Value,
    pub body: String,
    pub body_json: Value,
    pub elapsed: Duration,
}

pub async fn execute_request(
    client: &reqwest::Client,
    base_url: &str,
    spec: &RequestSpec,
    engine: &TemplateEngine,
    default_timeout: Duration,
    default_headers: &indexmap::IndexMap<String, String>,
) -> Result<StepResponse, CoreError> {
    execute_request_with_evidence(
        client,
        base_url,
        spec,
        engine,
        default_timeout,
        default_headers,
        &mut ExecutionCollector::new(false),
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_request_with_evidence(
    client: &reqwest::Client,
    base_url: &str,
    spec: &RequestSpec,
    engine: &TemplateEngine,
    default_timeout: Duration,
    default_headers: &indexmap::IndexMap<String, String>,
    evidence: &mut ExecutionCollector,
    event: Option<usize>,
) -> Result<StepResponse, CoreError> {
    let url = match (&spec.url, &spec.path) {
        (Some(u), _) => engine.render_str(u)?,
        (None, Some(p)) => {
            let p = engine.render_str(p)?;
            format!(
                "{}/{}",
                base_url.trim_end_matches('/'),
                p.trim_start_matches('/')
            )
        }
        (None, None) => return Err(CoreError::Http("request has no path/url".into())),
    };

    let method: reqwest::Method = spec
        .method
        .to_uppercase()
        .parse()
        .map_err(|_| CoreError::Http(format!("bad method `{}`", spec.method)))?;

    let mut req = client
        .request(method, &url)
        .timeout(spec.timeout.unwrap_or(default_timeout));

    for (k, v) in default_headers {
        req = req.header(k, v);
    }
    for (k, v) in &spec.headers {
        let rendered = engine.render_value(v)?;
        let text = rendered
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| rendered.to_string());
        req = req.header(k, text);
    }

    if !spec.query.is_empty() {
        let mut pairs = Vec::new();
        for (k, v) in &spec.query {
            let rendered = engine.render_value(v)?;
            let text = rendered
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| rendered.to_string());
            pairs.push((k.clone(), text));
        }
        req = req.query(&pairs);
    }

    if let Some(json) = &spec.json {
        req = req.json(&engine.render_value(json)?);
    } else if let Some(body) = &spec.body {
        req = req.body(engine.render_str(body)?);
    } else if let Some(form) = &spec.form {
        let mut rendered = Vec::new();
        for (k, v) in form {
            rendered.push((k.clone(), engine.render_str(v)?));
        }
        req = req.form(&rendered);
    }

    // Inspect and send the same built request: reporting must never evaluate
    // uuid(), random_int(), or any other request template a second time.
    let request = req
        .build()
        .map_err(|e| CoreError::Http(format!("{url}: {e}")))?;
    if evidence.collects_sources() {
        let headers = header_evidence(request.headers());
        let query: Vec<_> = request
            .url()
            .query_pairs()
            .map(|(key, value)| json!({"name": key, "value": value}))
            .collect();
        let body = request
            .body()
            .and_then(|body| body.as_bytes())
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
        let body_value = body
            .as_deref()
            .map(|body| serde_json::from_str::<Value>(body).unwrap_or_else(|_| json!(body)));
        evidence.request(
            event,
            json!({
                "method": request.method().as_str(),
                "url": request.url().as_str(),
                "query": query,
                "headers": headers,
                "body": body_value,
                "timeout_ms": request.timeout().map(|duration| duration.as_millis() as u64),
            }),
        );
    }
    // Payload collection is outside the HTTP latency measured by assertions.
    let started = Instant::now();
    let resp = client
        .execute(request)
        .await
        .map_err(|e| CoreError::Http(format!("{url}: {e}")))?;
    let status = resp.status().as_u16();
    let mut headers = Map::new();
    for (k, v) in resp.headers() {
        headers.insert(
            k.to_string(),
            Value::String(v.to_str().unwrap_or("").to_string()),
        );
    }
    let report_headers = evidence
        .collects_sources()
        .then(|| header_evidence(resp.headers()));
    if let Some(headers) = &report_headers {
        evidence.source(json!({"headers": headers}));
    }
    if event.is_some() {
        evidence.detail(
            event,
            "response",
            json!({
                "status": status, "headers": report_headers,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "body": null,
                "body_received": false,
            }),
        );
    }
    let body = resp
        .text()
        .await
        .map_err(|e| CoreError::Http(format!("{url}: {e}")))?;
    let body_json = serde_json::from_str(&body).unwrap_or(Value::Null);
    let elapsed = started.elapsed();
    if event.is_some() {
        evidence.detail(
            event,
            "response",
            json!({
                "status": status, "headers": report_headers,
                "elapsed_ms": elapsed.as_millis() as u64,
                "body": serde_json::from_str::<Value>(&body).unwrap_or_else(|_| json!(body)),
                "body_received": true,
            }),
        );
    }

    Ok(StepResponse {
        status,
        headers: Value::Object(headers),
        body,
        body_json,
        elapsed,
    })
}

fn header_evidence(headers: &reqwest::header::HeaderMap) -> Map<String, Value> {
    let mut values = Map::new();
    for (name, value) in headers {
        let value = json!(value.to_str().unwrap_or(""));
        match values.entry(name.to_string()) {
            serde_json::map::Entry::Vacant(entry) => {
                entry.insert(value);
            }
            serde_json::map::Entry::Occupied(mut entry) => {
                let previous = entry.get_mut();
                if let Some(values) = previous.as_array_mut() {
                    values.push(value);
                } else {
                    *previous = json!([previous.clone(), value]);
                }
            }
        }
    }
    values
}
