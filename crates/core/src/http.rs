use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use vault_dsl::RequestSpec;

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

    let started = Instant::now();
    let resp = req
        .send()
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
    let body = resp
        .text()
        .await
        .map_err(|e| CoreError::Http(format!("{url}: {e}")))?;
    let body_json = serde_json::from_str(&body).unwrap_or(Value::Null);

    Ok(StepResponse {
        status,
        headers: Value::Object(headers),
        body,
        body_json,
        elapsed: started.elapsed(),
    })
}
