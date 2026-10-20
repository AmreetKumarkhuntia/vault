use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::Router;
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::record::{RecordedExchange, RecordedRequest};
use crate::session::{resolve_dep, response_for, Session, SessionGuard};
use crate::UNMATCHED_STATUS;

type Active = Arc<Mutex<Option<Arc<Session>>>>;

pub struct MockServer {
    active: Active,
    addr: SocketAddr,
}

#[derive(Debug, thiserror::Error)]
pub enum MockError {
    #[error("mock listener bind failed on {0}: {1}")]
    Bind(String, std::io::Error),
}

impl MockServer {
    /// Bound once per run: the black-box target reads its dependency URLs at
    /// startup, so the address must stay stable across tests.
    pub async fn start(bind: &str) -> Result<Self, MockError> {
        let active: Active = Arc::new(Mutex::new(None));
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .map_err(|e| MockError::Bind(bind.to_string(), e))?;
        let addr = listener
            .local_addr()
            .map_err(|e| MockError::Bind(bind.to_string(), e))?;
        let app = Router::new().fallback(handle).with_state(active.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { active, addr })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn arm(&self, session: Session) -> SessionGuard {
        let session = Arc::new(session);
        *self.active.lock() = Some(session.clone());
        SessionGuard {
            active: self.active.clone(),
            session,
            base_url: self.base_url(),
        }
    }
}

async fn handle(State(active): State<Active>, req: Request) -> Response {
    let method = req.method().to_string();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let query = parse_query(uri.query().unwrap_or(""));
    let mut header_map = serde_json::Map::new();
    for (k, v) in req.headers() {
        insert_multi(
            &mut header_map,
            k.to_string(),
            json!(v.to_str().unwrap_or("")),
        );
    }
    let headers = Value::Object(header_map);
    let body_bytes = axum::body::to_bytes(req.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let body = String::from_utf8_lossy(&body_bytes).to_string();
    let body_json = serde_json::from_str(&body).unwrap_or(Value::Null);

    let Some(session) = active.lock().clone() else {
        return plain(
            UNMATCHED_STATUS,
            json!({"error": "vault-mock: no active session"}),
        );
    };

    let Some((dep, rel_path)) = resolve_dep(&session, &path) else {
        session.record(RecordedExchange {
            seq: 0,
            at_unix_ms: now_ms(),
            dependency: "<unknown>".into(),
            request: RecordedRequest {
                method,
                path,
                query,
                headers,
                body,
                body_json,
            },
            matched_stub: None,
            responded_status: UNMATCHED_STATUS,
            render_error: None,
        });
        return plain(
            UNMATCHED_STATUS,
            json!({"error": "vault-mock: no dependency prefix matched", "path": uri.path()}),
        );
    };
    let dep_name = dep.name.clone();

    let recorded = RecordedRequest {
        method,
        path: rel_path,
        query,
        headers,
        body,
        body_json,
    };

    for stub in &dep.stubs {
        if !crate::matcher::request_matches(&stub.spec.match_, &recorded) {
            continue;
        }
        let Some(serve_index) = stub.take_slot() else {
            continue;
        };
        let Some(resp) = response_for(stub, serve_index) else {
            continue;
        };

        let path_params = stub
            .spec
            .match_
            .path
            .as_ref()
            .and_then(|p| crate::matcher::path_matches(p, &recorded.path))
            .unwrap_or_else(|| json!({}));
        let request_ctx = json!({
            "path": recorded.path,
            "path_params": path_params,
            "query": recorded.query,
            "headers": recorded.headers,
            "body": recorded.body_json,
        });
        let rendered = match (session.render)(&resp, request_ctx) {
            Ok(rendered) => rendered,
            Err(detail) => {
                session.record(RecordedExchange {
                    seq: 0,
                    at_unix_ms: now_ms(),
                    dependency: dep_name.clone(),
                    request: recorded,
                    matched_stub: Some(stub.name.clone()),
                    responded_status: UNMATCHED_STATUS,
                    render_error: Some(detail.clone()),
                });
                return plain(
                    UNMATCHED_STATUS,
                    json!({"error": "vault-mock: stub response template failed", "stub": stub.name, "detail": detail}),
                );
            }
        };

        let status = rendered["status"].as_u64().unwrap_or(200) as u16;
        session.record(RecordedExchange {
            seq: 0,
            at_unix_ms: now_ms(),
            dependency: dep_name,
            request: recorded,
            matched_stub: Some(stub.name.clone()),
            responded_status: status,
            render_error: None,
        });

        if let Some(ms) = rendered["latency_ms"].as_u64() {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
        return build_response(&rendered);
    }

    session.record(RecordedExchange {
        seq: 0,
        at_unix_ms: now_ms(),
        dependency: dep_name.clone(),
        request: recorded,
        matched_stub: None,
        responded_status: UNMATCHED_STATUS,
        render_error: None,
    });
    plain(
        UNMATCHED_STATUS,
        json!({"error": "vault-mock: no stub matched", "dependency": dep_name}),
    )
}

fn build_response(rendered: &Value) -> Response {
    let status = rendered["status"].as_u64().unwrap_or(200) as u16;
    let mut builder = Response::builder().status(status);
    let mut has_content_type = false;
    if let Some(headers) = rendered["headers"].as_object() {
        for (k, v) in headers {
            if k.eq_ignore_ascii_case("content-type") {
                has_content_type = true;
            }
            builder = builder.header(k, text(v));
        }
    }
    let body = if !rendered["json"].is_null() {
        if !has_content_type {
            builder = builder.header("content-type", "application/json");
        }
        rendered["json"].to_string()
    } else {
        text(&rendered["body"])
    };
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| plain(500, json!({"error": "bad stub"})))
}

/// A lone `{{ expr }}` renders to its native JSON type; headers and bodies need its text.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn plain(status: u16, body: Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn parse_query(q: &str) -> Value {
    let mut m = serde_json::Map::new();
    for pair in q.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        insert_multi(&mut m, url_decode(k), Value::String(url_decode(v)));
    }
    Value::Object(m)
}

/// A repeated key becomes an array instead of silently keeping the last value.
fn insert_multi(m: &mut serde_json::Map<String, Value>, key: String, val: Value) {
    match m.get_mut(&key) {
        Some(Value::Array(items)) => items.push(val),
        Some(existing) => *existing = Value::Array(vec![existing.take(), val]),
        None => {
            m.insert(key, val);
        }
    }
}

fn url_decode(s: &str) -> String {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
