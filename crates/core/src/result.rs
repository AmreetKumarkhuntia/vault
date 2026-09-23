use serde::Serialize;
use serde_json::{Map, Value};
use vault_store::VerifyOutcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TestStatus {
    Passed,
    Failed,
    Errored,
    Skipped,
}

impl TestStatus {
    pub fn worst(self, other: TestStatus) -> TestStatus {
        use TestStatus::*;
        match (self, other) {
            (Errored, _) | (_, Errored) => Errored,
            (Failed, _) | (_, Failed) => Failed,
            (Passed, s) | (s, Passed) => s,
            (Skipped, Skipped) => Skipped,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ResponseSummary {
    pub status: u16,
    pub elapsed_ms: u64,
    pub body: Value,
}

#[derive(Debug, Serialize)]
pub struct StepResult {
    pub name: String,
    pub status: TestStatus,
    pub response: Option<ResponseSummary>,
    pub checks: VerifyOutcome,
    pub attempts: u32,
}

#[derive(Debug, Serialize)]
pub struct TestResult {
    pub name: String,
    pub status: TestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub steps: Vec<StepResult>,
    pub verify: VerifyOutcome,
    pub seed_receipts: Vec<String>,
    pub captures: Map<String, Value>,
    /// Full mock recording log, kept only for non-passing tests.
    pub recorded_calls: Vec<Value>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow: Option<String>,
}

impl TestResult {
    pub fn skipped(name: &str, reason: String) -> Self {
        Self {
            name: name.to_string(),
            status: TestStatus::Skipped,
            skip_reason: Some(reason),
            error: None,
            steps: vec![],
            verify: VerifyOutcome::default(),
            seed_receipts: vec![],
            captures: Map::new(),
            recorded_calls: vec![],
            duration_ms: 0,
            flow: None,
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct RunResult {
    pub schema_version: u32,
    pub environment: String,
    pub tests: Vec<TestResult>,
    pub duration_ms: u64,
}

impl RunResult {
    pub fn count(&self, status: TestStatus) -> usize {
        self.tests.iter().filter(|t| t.status == status).count()
    }

    pub fn status(&self) -> TestStatus {
        self.tests
            .iter()
            .map(|t| t.status)
            .fold(TestStatus::Passed, TestStatus::worst)
    }

    /// 0 all passed · 1 test failed · 3 environment error (preflight is the
    /// CLI's job; ERRORED mid-run maps here too).
    pub fn exit_code(&self) -> i32 {
        match self.status() {
            TestStatus::Passed | TestStatus::Skipped => 0,
            TestStatus::Failed => 1,
            TestStatus::Errored => 3,
        }
    }
}
