use serde::Serialize;
use serde_json::Value;

/// Result of evaluating one verify document against a store (or the mock log).
/// Assertion failures are DATA here, never `Err` — `Err` is reserved for
/// harness faults (connection lost, type error).
#[derive(Debug, Default, Serialize)]
pub struct VerifyOutcome {
    pub checks: Vec<CheckResult>,
}

impl VerifyOutcome {
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| matches!(c, CheckResult::Pass { .. }))
    }
    pub fn failures(&self) -> impl Iterator<Item = &CheckFailure> {
        self.checks.iter().filter_map(|c| match c {
            CheckResult::Fail(f) => Some(f),
            _ => None,
        })
    }
    pub fn push_pass(&mut self, description: impl Into<String>) {
        self.checks.push(CheckResult::Pass { description: description.into() });
    }
    pub fn merge(&mut self, other: VerifyOutcome) {
        self.checks.extend(other.checks);
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CheckResult {
    Pass { description: String },
    Fail(CheckFailure),
}

#[derive(Debug, Serialize)]
pub struct CheckFailure {
    /// Human phrasing of the expectation ("table `orders`: expected row user_id=1 …").
    pub description: String,
    /// Where in the YAML the expectation came from, e.g. "verify.postgres[0].expect[1]".
    pub yaml_path: String,
    pub expected: Value,
    pub kind: FailureKind,
    /// Ranked closest actual rows / calls / values.
    pub near_misses: Vec<NearMiss>,
    /// Poll count when `eventually:` was in play (1 = single shot).
    pub attempts: u32,
    pub elapsed_ms: u64,
}

impl CheckFailure {
    pub fn new(
        description: impl Into<String>,
        yaml_path: impl Into<String>,
        expected: Value,
        kind: FailureKind,
    ) -> Self {
        Self {
            description: description.into(),
            yaml_path: yaml_path.into(),
            expected,
            kind,
            near_misses: Vec::new(),
            attempts: 1,
            elapsed_ms: 0,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FailureKind {
    /// The headline feature: an expected insertion that never happened.
    MissingRow,
    UnexpectedRow { actual: Value },
    UnexpectedChange { before: Value, after: Value },
    ValueMismatch { diffs: Vec<FieldDiff> },
    MissingKey,
    UnexpectedKey { key: String },
    CountMismatch { expected: String, actual: u64 },
    MissedCall { satisfied: u64 },
    UnexpectedCall { exchange: Value },
    OrderViolation { interleaving: Vec<(String, u64)> },
}

#[derive(Debug, Clone, Serialize)]
pub struct NearMiss {
    pub actual: Value,
    pub diffs: Vec<FieldDiff>,
    /// matched columns / total columns, 0.0..=1.0
    pub score: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldDiff {
    pub path: String,
    pub expected: Value,
    pub actual: Value,
}
