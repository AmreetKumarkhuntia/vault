use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::Value;
use vault_dsl::{MockDep, UnmatchedPolicy};

use crate::record::RecordedExchange;
use crate::server::{Active, PrefixClaim};

/// Renders one stub response for one request. The engine supplies it so stub
/// templates follow the same rules as every other template in the test.
pub type ResponseRenderer = Arc<dyn Fn(&Value, Value) -> Result<Value, String> + Send + Sync>;

/// Per-test mock state: the armed stub table plus the recording log.
pub struct Session {
    pub key: String,
    /// Flow the test runs in, so an orphan candidate maps back to its item.
    pub flow: Option<String>,
    pub deps: IndexMap<String, ArmedDep>,
    pub default_unmatched: UnmatchedPolicy,
    pub log: Mutex<Vec<RecordedExchange>>,
    pub seq: AtomicU64,
    pub last_recorded_at_ms: AtomicU64,
    pub render: ResponseRenderer,
}

pub struct ArmedDep {
    pub name: String,
    /// Path prefix on the shared listener, e.g. "/payments".
    pub prefix: String,
    pub unmatched: Option<UnmatchedPolicy>,
    pub stubs: Vec<ArmedStub>,
    pub mode: crate::server::ClaimMode,
}

pub struct ArmedStub {
    pub name: String,
    pub spec: vault_dsl::Stub,
    /// How many times this stub has served (drives `times` and `responses`).
    pub served: AtomicU32,
}

impl ArmedStub {
    /// Claims the next serve slot in one atomic step: check-then-increment
    /// would let concurrent calls overshoot `times`.
    pub fn take_slot(&self) -> Option<u32> {
        self.served
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                match self.spec.times {
                    Some(t) if n >= t => None,
                    _ => Some(n + 1),
                }
            })
            .ok()
    }
}

impl Session {
    pub fn new(
        key: &str,
        mocks: &IndexMap<String, MockDep>,
        default_unmatched: UnmatchedPolicy,
        render: ResponseRenderer,
    ) -> Self {
        let deps = mocks
            .iter()
            .map(|(name, dep)| {
                let armed = ArmedDep {
                    name: name.clone(),
                    prefix: dep.mount(name),
                    unmatched: dep.unmatched.clone(),
                    mode: crate::server::claim_mode(dep, &default_unmatched),
                    stubs: dep
                        .stubs
                        .iter()
                        .enumerate()
                        .map(|(i, s)| ArmedStub {
                            name: s.name.clone().unwrap_or_else(|| format!("stubs[{i}]")),
                            spec: s.clone(),
                            served: AtomicU32::new(0),
                        })
                        .collect(),
                };
                (name.clone(), armed)
            })
            .collect();
        Self {
            key: key.to_string(),
            flow: None,
            deps,
            default_unmatched,
            log: Mutex::new(Vec::new()),
            seq: AtomicU64::new(0),
            last_recorded_at_ms: AtomicU64::new(0),
            render,
        }
    }

    pub fn record(&self, mut exchange: RecordedExchange) {
        exchange.seq = self.seq.fetch_add(1, Ordering::SeqCst);
        self.last_recorded_at_ms
            .store(exchange.at_unix_ms as u64, Ordering::SeqCst);
        self.log.lock().push(exchange);
    }

    pub fn unmatched_policy(&self, dep: &str) -> UnmatchedPolicy {
        self.deps
            .get(dep)
            .and_then(|d| d.unmatched.clone())
            .unwrap_or_else(|| self.default_unmatched.clone())
    }
}

#[derive(Debug)]
pub struct SessionReport {
    pub recordings: Vec<RecordedExchange>,
    pub mocked_deps: Vec<String>,
    /// Dependencies whose unmatched policy was `fail` and that received
    /// at least one unmatched request.
    pub unmatched_hits: Vec<RecordedExchange>,
    /// Dependencies this test forbids: empty mounts with a `fail` policy.
    pub forbidden_deps: Vec<String>,
    pub base_urls: IndexMap<String, String>,
}

/// Disarms the session on Drop — survives panics and Ctrl-C paths where the
/// engine unwinds.
pub struct SessionGuard {
    pub(crate) active: Active,
    pub(crate) session: Arc<Session>,
    pub(crate) base_url: String,
    /// Dropped after the session is removed, so the next claimant never routes to it.
    pub(crate) _claim: PrefixClaim,
}

impl SessionGuard {
    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    pub fn base_urls(&self) -> IndexMap<String, String> {
        self.session
            .deps
            .values()
            .map(|d| (d.name.clone(), format!("{}{}", self.base_url, d.prefix)))
            .collect()
    }

    pub fn drain(&self) -> SessionReport {
        let recordings = self.session.log.lock().clone();
        let unmatched_hits = recordings
            .iter()
            .filter(|r| {
                // A stub whose template failed is reported whatever the unmatched policy says.
                r.render_error.is_some()
                    || (r.matched_stub.is_none()
                        && self.session.unmatched_policy(&r.dependency) == UnmatchedPolicy::Fail)
            })
            .cloned()
            .collect();
        SessionReport {
            mocked_deps: self.session.deps.keys().cloned().collect(),
            unmatched_hits,
            forbidden_deps: self
                .session
                .deps
                .values()
                .filter(|d| d.mode == crate::server::ClaimMode::Forbid)
                .map(|d| d.name.clone())
                .collect(),
            base_urls: self.base_urls(),
            recordings,
        }
    }

    pub fn last_recorded_at_ms(&self) -> u64 {
        self.session.last_recorded_at_ms.load(Ordering::SeqCst)
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.active
            .lock()
            .sessions
            .retain(|armed| !Arc::ptr_eq(armed, &self.session));
    }
}

/// Resolve which dependency a request path belongs to; returns the dep name
/// and the path relative to its prefix.
pub fn resolve_dep<'s>(session: &'s Session, path: &str) -> Option<(&'s ArmedDep, String)> {
    // Longest prefix wins so `/api/billing` is reachable next to `/api`.
    session
        .deps
        .values()
        .filter_map(|dep| {
            let rest = path.strip_prefix(&dep.prefix)?;
            match rest {
                "" => Some((dep, "/".to_string())),
                r if r.starts_with('/') => Some((dep, r.to_string())),
                _ => None,
            }
        })
        .max_by_key(|(dep, _)| dep.prefix.len())
}

pub fn response_for(stub: &ArmedStub, serve_index: u32) -> Option<Value> {
    let spec = &stub.spec;
    let chosen = if let Some(seq) = &spec.responses {
        let idx = (serve_index as usize).min(seq.len().saturating_sub(1));
        seq.get(idx)
    } else {
        spec.response.as_ref()
    }?;
    serde_json::to_value(SerializableResponse {
        status: chosen.status,
        headers: chosen.headers.clone(),
        json: chosen.json.clone(),
        body: chosen.body.clone(),
        latency_ms: chosen.latency.map(|d| d.as_millis() as u64),
    })
    .ok()
}

#[derive(serde::Serialize)]
struct SerializableResponse {
    status: u16,
    headers: IndexMap<String, String>,
    json: Option<Value>,
    body: Option<String>,
    latency_ms: Option<u64>,
}
