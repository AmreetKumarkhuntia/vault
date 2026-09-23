use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

// ---------------------------------------------------------------------------
// Global config: vault.yaml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    pub environments: IndexMap<String, Environment>,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub report: ReportCfg,
}

fn default_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Environment {
    pub target: TargetCfg,
    #[serde(default)]
    pub mock_server: MockServerCfg,
    /// Everything else is a store instance keyed by kind: postgres, redis, …
    #[serde(flatten)]
    pub stores: IndexMap<String, StoreCfg>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetCfg {
    pub base_url: String,
    #[serde(default)]
    pub health_check: Option<HealthCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheck {
    pub path: String,
    #[serde(default = "default_health_timeout", with = "humantime_serde")]
    pub timeout: Duration,
}

fn default_health_timeout() -> Duration {
    Duration::from_secs(30)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockServerCfg {
    #[serde(default = "default_mock_bind")]
    pub bind: String,
}

impl Default for MockServerCfg {
    fn default() -> Self {
        Self {
            bind: default_mock_bind(),
        }
    }
}

fn default_mock_bind() -> String {
    "127.0.0.1:0".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreCfg {
    pub url: String,
    /// Driver-specific options (reset excludes, etc.).
    #[serde(flatten)]
    pub options: IndexMap<String, Json>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default)]
    pub request: RequestDefaults,
    #[serde(default)]
    pub mock: MockDefaults,
    /// Reset specs keyed by store kind — driver-owned documents.
    #[serde(default)]
    pub reset: IndexMap<String, Json>,
    #[serde(default)]
    pub verify: VerifyDefaults,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestDefaults {
    #[serde(default = "default_request_timeout", with = "humantime_serde")]
    pub timeout: Duration,
    #[serde(default)]
    pub headers: IndexMap<String, String>,
}

impl Default for RequestDefaults {
    fn default() -> Self {
        Self {
            timeout: default_request_timeout(),
            headers: IndexMap::new(),
        }
    }
}

fn default_request_timeout() -> Duration {
    Duration::from_secs(10)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockDefaults {
    #[serde(default)]
    pub unmatched: UnmatchedPolicy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum UnmatchedPolicy {
    #[default]
    Fail,
    Allow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyDefaults {
    #[serde(default = "default_settle", with = "humantime_serde")]
    pub settle: Duration,
    #[serde(default = "default_poll", with = "humantime_serde")]
    pub poll_interval: Duration,
}

impl Default for VerifyDefaults {
    fn default() -> Self {
        Self {
            settle: default_settle(),
            poll_interval: default_poll(),
        }
    }
}

fn default_settle() -> Duration {
    Duration::from_millis(500)
}

fn default_poll() -> Duration {
    Duration::from_millis(100)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportCfg {
    #[serde(default)]
    pub json: Option<String>,
    #[serde(default)]
    pub junit: Option<String>,
}

// ---------------------------------------------------------------------------
// Test files: *.test.yaml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestDef {
    pub test: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub skip: Skip,
    #[serde(default = "default_test_timeout", with = "humantime_serde")]
    pub timeout: Duration,
    #[serde(default)]
    pub vars: IndexMap<String, Json>,
    /// Tables to watch for unexpected changes (postgres watch mode).
    #[serde(default)]
    pub watch: Vec<String>,
    /// Seed docs keyed by store kind. Driver-owned schema.
    #[serde(default)]
    pub seed: IndexMap<String, Json>,
    #[serde(default)]
    pub mocks: IndexMap<String, MockDep>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub verify: VerifyBlock,
}

fn default_test_timeout() -> Duration {
    Duration::from_secs(60)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Skip {
    #[default]
    No,
    Flag(bool),
    Reason(String),
}

impl Skip {
    pub fn reason(&self) -> Option<String> {
        match self {
            Skip::No | Skip::Flag(false) => None,
            Skip::Flag(true) => Some("skipped".into()),
            // Empty string = not skipped, so `skip: "${env.FLAG:-reason}"`
            // can gate a test on an environment variable.
            Skip::Reason(r) if r.is_empty() => None,
            Skip::Reason(r) => Some(r.clone()),
        }
    }
}

// ---- Mocks ----------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockDep {
    /// Path prefix on the shared listener; defaults to `/<name>`.
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub unmatched: Option<UnmatchedPolicy>,
    #[serde(default)]
    pub stubs: Vec<Stub>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stub {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "match")]
    pub match_: MatchSpec,
    #[serde(default)]
    pub response: Option<ResponseSpec>,
    /// Plural = sequenced responses; a cursor advances per received call.
    #[serde(default)]
    pub responses: Option<Vec<ResponseSpec>>,
    /// Serving budget; an exhausted stub stops matching.
    #[serde(default)]
    pub times: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchSpec {
    #[serde(default)]
    pub method: Option<String>,
    /// Exact path, `/v1/things/{id}` (params captured), or `{regex: ...}`.
    #[serde(default)]
    pub path: Option<Json>,
    /// Subset match; value "*" means "key present".
    #[serde(default)]
    pub query: IndexMap<String, Json>,
    /// Subset match, case-insensitive names.
    #[serde(default)]
    pub headers: IndexMap<String, Json>,
    /// `{json_partial: {...}}` | `{json_exact: {...}}` | `{regex: "..."}`.
    #[serde(default)]
    pub body: Option<Json>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseSpec {
    #[serde(default = "default_status")]
    pub status: u16,
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    #[serde(default)]
    pub json: Option<Json>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default, with = "humantime_serde::option")]
    pub latency: Option<Duration>,
}

fn default_status() -> u16 {
    200
}

// ---- Steps ----------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    pub request: RequestSpec,
    #[serde(default)]
    pub expect: Option<ExpectSpec>,
    #[serde(default)]
    pub capture: IndexMap<String, CaptureSpec>,
    /// Re-run the step until `expect` passes (HTTP-level polling).
    #[serde(default)]
    pub repeat: Option<RepeatSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestSpec {
    pub method: String,
    /// Joined to target.base_url.
    #[serde(default)]
    pub path: Option<String>,
    /// Absolute alternative to `path`.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub query: IndexMap<String, Json>,
    #[serde(default)]
    pub headers: IndexMap<String, Json>,
    #[serde(default)]
    pub json: Option<Json>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub form: Option<IndexMap<String, String>>,
    #[serde(default, with = "humantime_serde::option")]
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectSpec {
    /// `201` | `"2xx"` | `[200, 201]`
    #[serde(default)]
    pub status: Option<Json>,
    /// Subset; values are matchers (also `absent`).
    #[serde(default)]
    pub headers: IndexMap<String, Json>,
    #[serde(default)]
    pub json_partial: Option<Json>,
    #[serde(default)]
    pub json_exact: Option<Json>,
    /// Paths ignored by json_exact, e.g. `$.created_at`.
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub jsonpath: Vec<JsonPathAssert>,
    /// Non-JSON bodies: literal string or `{regex: ...}`.
    #[serde(default)]
    pub body: Option<Json>,
    #[serde(default)]
    pub time: Option<TimeAssert>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonPathAssert {
    pub path: String,
    /// Remaining keys are matcher operators: eq/ne/gt/regex/exists/type/…
    #[serde(flatten)]
    pub ops: IndexMap<String, Json>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeAssert {
    #[serde(with = "humantime_serde")]
    pub under: Duration,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureSpec {
    #[serde(default)]
    pub jsonpath: Option<String>,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub status: Option<bool>,
    /// `body: text` captures the raw body.
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub regex: Option<RegexCapture>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegexCapture {
    /// "body" (default) or "header:<Name>"
    #[serde(default)]
    pub on: Option<String>,
    pub pattern: String,
    #[serde(default = "default_group")]
    pub group: usize,
}

fn default_group() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepeatSpec {
    #[serde(with = "humantime_serde")]
    pub every: Duration,
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
}

// ---- Verify ----------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifyBlock {
    #[serde(default)]
    pub calls: Vec<CallExpect>,
    /// Order groups: names of call expectations that must appear as a
    /// subsequence of the recording log.
    #[serde(default)]
    pub ordered: Vec<Vec<String>>,
    #[serde(default)]
    pub unexpected: Option<UnmatchedPolicy>,
    /// Store verify docs keyed by kind (postgres, redis, …). Driver-owned.
    #[serde(flatten)]
    pub stores: IndexMap<String, Json>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallExpect {
    #[serde(default)]
    pub name: Option<String>,
    pub dependency: String,
    #[serde(rename = "match", default)]
    pub match_: MatchSpec,
    #[serde(default)]
    pub body: Option<Json>,
    /// `1` | `{gte: 1}` | `0` ("never called"). Default: `{gte: 1}`.
    #[serde(default)]
    pub count: Option<Json>,
}

impl CallExpect {
    pub fn label(&self, idx: usize) -> String {
        self.name.clone().unwrap_or_else(|| {
            format!(
                "calls[{idx}] {} {}",
                self.match_.method.as_deref().unwrap_or("*"),
                self.match_
                    .path
                    .as_ref()
                    .and_then(|p| p.as_str())
                    .unwrap_or("*")
            )
        })
    }
}

// ---------------------------------------------------------------------------
// Flow files: *.flow.yaml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowDef {
    pub flow: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub reset: FlowReset,
    pub stages: Vec<FlowStage>,
    #[serde(default)]
    pub on_failure: FlowOnFailure,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FlowReset {
    /// RESET before stage 1 only — the flow is the isolation unit.
    #[default]
    Once,
    /// Normal per-test isolation.
    Each,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum FlowOnFailure {
    #[default]
    SkipRest,
    Continue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowStage {
    pub test: String,
    /// Captures promoted into `{{ flow.* }}` scope.
    #[serde(default)]
    pub export: Vec<String>,
    /// Injected into the test's vars (overrides defaults).
    #[serde(default)]
    pub with: IndexMap<String, Json>,
}
