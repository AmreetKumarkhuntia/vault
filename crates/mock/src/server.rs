use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::Router;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Notify;
use vault_dsl::{MockDep, UnmatchedPolicy};

use crate::record::{RecordedExchange, RecordedRequest};
use crate::session::{resolve_dep, response_for, Session, SessionGuard};
use crate::UNMATCHED_STATUS;

/// How a test uses a mount. A stubbed (or `unmatched: allow`) mount answers
/// calls, so it is exclusive; a forbidden mount only records violations, so
/// any number of tests can forbid the same prefix at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimMode {
    Provide,
    Forbid,
}

#[derive(Clone, Debug, Serialize)]
pub struct Claim {
    pub prefix: String,
    pub mode: ClaimMode,
}

/// An empty mount whose resolved `unmatched` policy is `fail` is a Forbid;
/// everything else answers calls and is a Provide.
pub fn claim_mode(dep: &MockDep, default_unmatched: &UnmatchedPolicy) -> ClaimMode {
    let policy = dep.unmatched.as_ref().unwrap_or(default_unmatched);
    if dep.stubs.is_empty() && *policy == UnmatchedPolicy::Fail {
        ClaimMode::Forbid
    } else {
        ClaimMode::Provide
    }
}

/// An outbound call no armed session claimed during a parallel run. Nobody is
/// blamed for it; the runner re-runs every candidate item serially instead.
#[derive(Clone, Debug, Serialize)]
pub struct Orphan {
    pub method: String,
    pub path: String,
    pub query: Value,
    pub at_unix_ms: i64,
    /// Every test armed when the call arrived.
    pub candidates: Vec<Candidate>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub test: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow: Option<String>,
}

fn conflicts(a: &Claim, b: &Claim) -> bool {
    mounts_overlap(&a.prefix, &b.prefix)
        && (a.mode == ClaimMode::Provide || b.mode == ClaimMode::Provide)
}

/// Every armed session, the claims currently held, and the claims still
/// queued in arrival order. Lanes share one listener, so a request is routed
/// by the longest mount that covers it.
#[derive(Default)]
pub(crate) struct Armed {
    pub(crate) sessions: Vec<Arc<Session>>,
    held: Vec<(u64, Claim)>,
    waiting: Vec<(u64, Vec<Claim>)>,
    next_ticket: u64,
    /// Above one lane an unclaimed call cannot be attributed to a session.
    parallel: bool,
    orphans: Vec<Orphan>,
}

pub(crate) type Active = Arc<Mutex<Armed>>;

pub struct MockServer {
    active: Active,
    released: Arc<Notify>,
    addr: SocketAddr,
}

/// A set of held claims, released on drop.
pub struct PrefixClaim {
    active: Active,
    released: Arc<Notify>,
    ticket: u64,
    claims: Vec<Claim>,
}

impl PrefixClaim {
    pub fn claims(&self) -> &[Claim] {
        &self.claims
    }
}

impl Drop for PrefixClaim {
    fn drop(&mut self) {
        self.active
            .lock()
            .held
            .retain(|(ticket, _)| *ticket != self.ticket);
        self.released.notify_waiters();
    }
}

/// Removes a queued claim when its `claim()` future is dropped before being
/// granted, so a cancelled lane never blocks the claims behind it.
struct Queued {
    active: Active,
    released: Arc<Notify>,
    ticket: u64,
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.active
            .lock()
            .waiting
            .retain(|(ticket, _)| *ticket != self.ticket);
        self.released.notify_waiters();
    }
}

/// Two mounts overlap when one is a segment-prefix of the other; the root
/// mount `""` covers every path.
pub fn mounts_overlap(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    match long.strip_prefix(short) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
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
        let active: Active = Arc::default();
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
        Ok(Self {
            active,
            released: Arc::new(Notify::new()),
            addr,
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Holds `claims` once none of them conflicts with a held claim or with a
    /// claim queued earlier: arrival order, so a stream of shared forbids can
    /// never starve a waiting stub owner. Callers claim before their clocks
    /// start so queueing never counts as test time.
    pub async fn claim(&self, claims: Vec<Claim>) -> PrefixClaim {
        let ticket = {
            let mut armed = self.active.lock();
            armed.next_ticket += 1;
            let ticket = armed.next_ticket;
            armed.waiting.push((ticket, claims.clone()));
            ticket
        };
        let _queued = Queued {
            active: self.active.clone(),
            released: self.released.clone(),
            ticket,
        };
        loop {
            // Registered before the check so a release in between is not missed.
            let released = self.released.notified();
            {
                let mut armed = self.active.lock();
                let blocked = claims.iter().any(|c| {
                    armed.held.iter().any(|(_, h)| conflicts(c, h))
                        || armed.waiting.iter().any(|(t, queued)| {
                            *t < ticket && queued.iter().any(|h| conflicts(c, h))
                        })
                });
                if !blocked {
                    armed.waiting.retain(|(t, _)| *t != ticket);
                    armed
                        .held
                        .extend(claims.iter().cloned().map(|c| (ticket, c)));
                    return PrefixClaim {
                        active: self.active.clone(),
                        released: self.released.clone(),
                        ticket,
                        claims,
                    };
                }
            }
            released.await;
        }
    }

    /// With several lanes an unclaimed call goes to the orphan log instead of a
    /// session; with one lane it lands in the only armed session, as a serial
    /// run always did.
    pub fn set_parallel(&self, parallel: bool) {
        self.active.lock().parallel = parallel;
    }

    pub fn drain_orphans(&self) -> Vec<Orphan> {
        std::mem::take(&mut self.active.lock().orphans)
    }

    pub fn arm(&self, session: Session, claim: PrefixClaim) -> SessionGuard {
        let session = Arc::new(session);
        self.active.lock().sessions.push(session.clone());
        SessionGuard {
            active: self.active.clone(),
            session,
            base_url: self.base_url(),
            _claim: claim,
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

    let sessions = active.lock().sessions.clone();
    if sessions.is_empty() {
        return plain(
            UNMATCHED_STATUS,
            json!({"error": "vault-mock: no active session"}),
        );
    }

    // A stubbed mount is exclusive, so at most one armed session can answer; any
    // number may forbid the path, and each of them records the violation.
    let covering: Vec<_> = sessions
        .iter()
        .filter_map(|s| resolve_dep(s, &path).map(|(dep, rel)| (s, dep, rel)))
        .collect();
    let provider = covering
        .iter()
        .find(|(_, dep, _)| dep.mode == ClaimMode::Provide);
    let Some((session, dep, rel_path)) = provider else {
        if !covering.is_empty() {
            for (session, dep, rel_path) in &covering {
                session.record(RecordedExchange {
                    seq: 0,
                    at_unix_ms: now_ms(),
                    dependency: dep.name.clone(),
                    request: RecordedRequest {
                        method: method.clone(),
                        path: rel_path.clone(),
                        query: query.clone(),
                        headers: headers.clone(),
                        body: body.clone(),
                        body_json: body_json.clone(),
                    },
                    matched_stub: None,
                    responded_status: UNMATCHED_STATUS,
                    render_error: None,
                });
            }
            return plain(
                UNMATCHED_STATUS,
                json!({"error": "vault-mock: dependency must not be called", "path": uri.path()}),
            );
        }
        {
            let mut armed = active.lock();
            if armed.parallel {
                // Any armed lane could have made this call; log it instead of blaming all of them.
                armed.orphans.push(Orphan {
                    method,
                    path,
                    query,
                    at_unix_ms: now_ms(),
                    candidates: sessions
                        .iter()
                        .map(|s| Candidate {
                            test: s.key.clone(),
                            flow: s.flow.clone(),
                        })
                        .collect(),
                });
                return plain(
                    UNMATCHED_STATUS,
                    json!({"error": "vault-mock: no dependency prefix matched", "path": uri.path()}),
                );
            }
        }
        // One lane: the only armed session made the call, so it is recorded there.
        let unknown = RecordedExchange {
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
        };
        for session in &sessions {
            session.record(unknown.clone());
        }
        return plain(
            UNMATCHED_STATUS,
            json!({"error": "vault-mock: no dependency prefix matched", "path": uri.path()}),
        );
    };
    let dep_name = dep.name.clone();

    let recorded = RecordedRequest {
        method,
        path: rel_path.clone(),
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
