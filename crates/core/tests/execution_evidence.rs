use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vault_core::{
    ExecutionStatus, Gate, GateDecision, NoopGate, PausePoint, PauseSnapshot, TestExecution,
    TestRunner, TestStatus,
};
use vault_dsl::{Defaults, FlowDef, TestDef};
use vault_mock::MockServer;
use vault_store::{
    CheckFailure, CheckResult, FailureKind, SeedReceipt, Snapshot, StateStore, StoreDoc,
    StoreError, Table, VerifyOpts, VerifyOutcome,
};

#[derive(Default)]
struct RecordingStore {
    resets: AtomicU32,
    seeds: AtomicU32,
    verifies: AtomicU32,
    fail_polls: u32,
    fail_seed: bool,
}

#[async_trait]
impl StateStore for RecordingStore {
    fn kind(&self) -> &'static str {
        "postgres"
    }
    fn alias(&self) -> &str {
        "postgres"
    }
    async fn ping(&self) -> Result<(), StoreError> {
        Ok(())
    }
    async fn reset(&self, _: &StoreDoc) -> Result<(), StoreError> {
        self.resets.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn seed(&self, _: &StoreDoc) -> Result<SeedReceipt, StoreError> {
        self.seeds.fetch_add(1, Ordering::SeqCst);
        if self.fail_seed {
            return Err(StoreError::Harness("seed failed".into()));
        }
        Ok(SeedReceipt {
            entries: vec!["postgres: fixture applied".into()],
        })
    }
    async fn snapshot(&self, _: &StoreDoc) -> Result<Snapshot, StoreError> {
        Ok(Value::Null)
    }
    async fn diff_snapshot(&self, _: &StoreDoc, _: &Snapshot) -> Result<VerifyOutcome, StoreError> {
        Ok(VerifyOutcome::default())
    }
    async fn verify(&self, _: &StoreDoc, _: &VerifyOpts) -> Result<VerifyOutcome, StoreError> {
        let call = self.verifies.fetch_add(1, Ordering::SeqCst);
        let mut outcome = VerifyOutcome::default();
        if call < self.fail_polls {
            outcome.checks.push(CheckResult::Fail(CheckFailure::new(
                "pending row",
                "verify.postgres",
                json!(1),
                FailureKind::CountMismatch {
                    expected: "1".into(),
                    actual: 0,
                },
            )));
        } else {
            outcome.push_pass("row present");
        }
        Ok(outcome)
    }
    async fn inspect(&self, _: &str) -> Result<Table, StoreError> {
        Ok(Table::default())
    }
}

async fn runner(store: Option<Arc<RecordingStore>>) -> TestRunner {
    let mock = Arc::new(MockServer::start("127.0.0.1:0").await.unwrap());
    let mut stores: IndexMap<String, Arc<dyn StateStore>> = IndexMap::new();
    if let Some(store) = store {
        stores.insert("postgres".into(), store);
    }
    let mut defaults = Defaults::default();
    defaults.verify.settle = Duration::from_millis(2);
    defaults.verify.poll_interval = Duration::from_millis(1);
    TestRunner {
        client: reqwest::Client::new(),
        target_base_url: mock.base_url().to_string(),
        defaults,
        global_seed: IndexMap::new(),
        stores,
        mock,
        gate: Arc::new(NoopGate),
        abort_run: std::sync::atomic::AtomicBool::new(false),
    }
}

fn definition(value: Value) -> TestDef {
    serde_json::from_value(value).unwrap()
}

fn echo_test() -> TestDef {
    definition(json!({
        "test": "echo",
        "mocks": { "target": { "stubs": [{
            "name": "echo",
            "match": { "method": "POST", "path": "/echo" },
            "response": { "status": 200, "json": {"id": "{{ request.body.id }}"} }
        }] } },
        "verify": {"calls": [{"dependency": "target", "match": {"path": "/echo"}, "count": 1}]},
        "steps": [{
            "name": "echo",
            "request": { "method": "POST", "path": "/target/echo", "json": {"id": "{{ uuid() }}"} },
            "expect": { "status": 200 },
            "capture": { "id": { "jsonpath": "$.id" } }
        }]
    }))
}

fn event<'a>(execution: &'a TestExecution, phase: &str) -> &'a vault_core::ExecutionEvent {
    execution
        .events
        .iter()
        .find(|event| event.phase == phase)
        .unwrap()
}

#[tokio::test]
async fn evidence_observes_the_same_request_and_keeps_passing_mock_calls() {
    let runner = runner(None).await;
    let def = echo_test();
    let (result, execution) = runner
        .run_test_with_evidence(&def, &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Passed);
    assert!(
        result.recorded_calls.is_empty(),
        "legacy passing result keeps its old policy"
    );
    let request = &event(&execution, "http_attempt").details["request"]["body"];
    let calls = &event(&execution, "finalization").details["recorded_calls"];
    assert_eq!(request, &calls[0]["request"]["body_json"]);
    assert_eq!(request["id"], result.captures["id"]);
    assert_eq!(event(&execution, "capture").details["value"], request["id"]);
    let legacy = serde_json::to_value(result).unwrap();
    assert!(legacy.get("events").is_none());
    assert!(legacy.get("execution").is_none());
    assert_eq!(
        serde_json::to_value(ExecutionStatus::NotRun).unwrap(),
        "NOT_RUN"
    );
}

#[tokio::test]
async fn repeat_attempts_keep_failed_and_successful_http_evidence() {
    let runner = runner(None).await;
    let def = definition(json!({
        "test": "repeat",
        "mocks": {"target": {"stubs": [{"match": {"path": "/repeat"}, "responses": [
            {"status": 503}, {"status": 200}
        ]}]}},
        "verify": {"calls": [{"dependency": "target", "match": {"path": "/repeat"}, "count": 2}]},
        "steps": [{"name": "poll", "request": {"method": "GET", "path": "/target/repeat"},
            "expect": {"status": 200}, "repeat": {"every": "1ms", "timeout": "1s"}}]
    }));
    let (result, execution) = runner
        .run_test_with_evidence(&def, &IndexMap::new(), false, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Passed);
    assert_eq!(result.steps[0].attempts, 2);
    let attempts: Vec<_> = execution
        .events
        .iter()
        .filter(|event| event.phase == "http_attempt")
        .collect();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].status, ExecutionStatus::Failed);
    assert_eq!(attempts[1].status, ExecutionStatus::Passed);
    assert_eq!(attempts[0].details["response"]["status"], 503);
    assert_eq!(attempts[1].attempts, 2);
}

#[tokio::test]
async fn setup_watch_and_successful_eventually_polls_are_recorded() {
    let store = Arc::new(RecordingStore {
        fail_polls: 1,
        ..Default::default()
    });
    let mut runner = runner(Some(store.clone())).await;
    runner
        .global_seed
        .insert("postgres".into(), json!([{"sql": "DO NOT EXPORT RAW SQL"}]));
    let def = definition(json!({
        "test": "store evidence", "watch": ["orders"],
        "seed": {"postgres": [{"sql": "ANOTHER RAW SQL"}]},
        "verify": {"postgres": [{"eventually": "1s"}]}
    }));
    let (result, execution) = runner
        .run_test_with_evidence(&def, &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Passed);
    assert_eq!(store.resets.load(Ordering::SeqCst), 1);
    assert_eq!(store.verifies.load(Ordering::SeqCst), 3);
    assert_eq!(event(&execution, "verify_store").attempts, 3);
    let polls: Vec<_> = execution
        .events
        .iter()
        .filter(|event| event.phase == "verify_poll")
        .collect();
    assert_eq!(polls.len(), 3);
    assert_eq!(polls[0].status, ExecutionStatus::Failed);
    assert_eq!(polls[2].details["settle_confirmation"], true);
    assert_eq!(event(&execution, "seed").details["global"], true);
    assert_eq!(event(&execution, "seed").details["local"], true);
    assert_eq!(
        event(&execution, "watch_diff").status,
        ExecutionStatus::Passed
    );
    let serialized = serde_json::to_string(&execution).unwrap();
    assert!(!serialized.contains("RAW SQL"));
}

#[tokio::test]
async fn failed_assertion_skips_later_steps_but_runs_verification() {
    let store = Arc::new(RecordingStore::default());
    let runner = runner(Some(store.clone())).await;
    let mut def = echo_test();
    def.steps[0].expect.as_mut().unwrap().status = Some(json!(201));
    let mut later = def.steps[0].clone();
    later.name = "later".into();
    def.steps.push(later);
    def.verify.stores.insert("postgres".into(), json!([]));
    let (result, execution) = runner
        .run_test_with_evidence(&def, &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Failed);
    assert_eq!(result.steps.len(), 1);
    let later = execution
        .events
        .iter()
        .find(|event| event.phase == "step" && event.step_index == Some(1))
        .unwrap();
    assert_eq!(later.status, ExecutionStatus::NotRun);
    assert!(later.start_offset_ms.is_none());
    assert_eq!(store.verifies.load(Ordering::SeqCst), 1);
    assert_eq!(
        event(&execution, "verify_store").status,
        ExecutionStatus::Passed
    );
}

#[tokio::test]
async fn capture_error_retains_prior_capture_and_response_without_changing_legacy_result() {
    let runner = runner(None).await;
    let mut def = echo_test();
    def.steps[0].capture.insert(
        "missing".into(),
        serde_json::from_value(json!({"jsonpath": "$.absent"})).unwrap(),
    );
    let (result, execution) = runner
        .run_test_with_evidence(&def, &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Errored);
    assert!(result.steps.is_empty());
    assert!(result.captures.is_empty());
    assert_eq!(
        event(&execution, "http_attempt").status,
        ExecutionStatus::Passed
    );
    let captures: Vec<_> = execution
        .events
        .iter()
        .filter(|event| event.phase == "capture")
        .collect();
    assert_eq!(captures[0].status, ExecutionStatus::Passed);
    assert_eq!(captures[1].status, ExecutionStatus::Errored);
    assert_eq!(event(&execution, "step").status, ExecutionStatus::Errored);
    assert_eq!(
        event(&execution, "finalization").details["recorded_calls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn timeout_preserves_request_and_mock_log_after_session_disarms() {
    let runner = runner(None).await;
    let mut def = echo_test();
    def.timeout = Duration::from_millis(100);
    def.mocks["target"].stubs[0]
        .response
        .as_mut()
        .unwrap()
        .latency = Some(Duration::from_secs(1));
    let (result, execution) = runner
        .run_test_with_evidence(&def, &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Errored);
    let attempt = event(&execution, "http_attempt");
    assert_eq!(attempt.status, ExecutionStatus::Errored);
    assert!(attempt.details["request"].is_object());
    assert_eq!(
        event(&execution, "finalization").details["recorded_calls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let response = runner
        .client
        .get(format!("{}/target/echo", runner.mock.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        599,
        "guard still disarms on timeout"
    );
}

#[tokio::test]
async fn timeout_retains_partial_response_headers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4096];
        assert!(socket.read(&mut buffer).await.unwrap() > 0);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Partial: yes\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    });
    let mut runner = runner(None).await;
    runner.target_base_url = format!("http://{address}");
    let def = definition(json!({"test": "partial", "timeout": "100ms", "steps": [
        {"name": "partial", "request": {"method": "GET", "path": "/"}}
    ]}));
    let (result, execution) = runner
        .run_test_with_evidence(&def, &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Errored);
    let response = &event(&execution, "http_attempt").details["response"];
    assert_eq!(response["status"], 200);
    assert_eq!(response["headers"]["x-partial"], "yes");
    assert_eq!(response["body_received"], false);
    server.abort();
}

#[tokio::test]
async fn flow_evidence_stays_aligned_with_skips_errors_exports_and_reset_once() {
    let store = Arc::new(RecordingStore::default());
    let runner = runner(Some(store.clone())).await;
    let skipped = definition(json!({"test": "skip", "skip": "not ready"}));
    let echo = echo_test();
    let flow: FlowDef = serde_json::from_value(json!({
        "flow": "identity", "on_failure": "continue", "stages": [
            {"test": "skip"}, {"test": "echo", "with": {"bad": "{{ ??? }}"}},
            {"test": "echo", "export": ["id"]}, {"test": "echo"}
        ]
    }))
    .unwrap();
    let tests = HashMap::from([(skipped.test.clone(), &skipped), (echo.test.clone(), &echo)]);
    let (outcome, executions) = runner.run_flow_with_evidence(&flow, &tests).await;
    assert_eq!(outcome.results.len(), 4);
    assert_eq!(executions.len(), 4);
    assert_eq!(outcome.results[0].status, TestStatus::Skipped);
    assert_eq!(outcome.results[1].status, TestStatus::Errored);
    assert_eq!(store.resets.load(Ordering::SeqCst), 1);
    for (index, execution) in executions.iter().enumerate() {
        assert_eq!(execution.flow_stage, Some(index));
    }
    assert_eq!(
        executions[2].exports["id"],
        outcome.results[2].captures["id"]
    );
    assert!(executions[3]
        .events
        .iter()
        .all(|event| event.phase != "reset"));
}

#[tokio::test]
async fn initialization_error_and_skipped_remainder_have_distinct_evidence() {
    let store = Arc::new(RecordingStore {
        fail_seed: true,
        ..Default::default()
    });
    let mut runner = runner(Some(store)).await;
    runner.global_seed.insert("postgres".into(), json!([]));
    let first = definition(json!({"test": "first"}));
    let second = definition(json!({"test": "second"}));
    let flow: FlowDef = serde_json::from_value(json!({
        "flow": "bad setup", "on_failure": "continue", "stages": [{"test": "first"}, {"test": "second"}]
    })).unwrap();
    let tests = HashMap::from([(first.test.clone(), &first), (second.test.clone(), &second)]);
    let (outcome, executions) = runner.run_flow_with_evidence(&flow, &tests).await;
    assert_eq!(outcome.results[0].status, TestStatus::Errored);
    assert_eq!(
        event(&executions[0], "reset").status,
        ExecutionStatus::Passed
    );
    assert_eq!(
        event(&executions[0], "seed").status,
        ExecutionStatus::Errored
    );
    assert_eq!(outcome.results[1].status, TestStatus::Skipped);
    assert_eq!(
        event(&executions[1], "stage").status,
        ExecutionStatus::Skipped
    );
    assert_eq!(executions[1].flow_stage, Some(1));
}

struct AbortAfterStep;

impl Gate for AbortAfterStep {
    fn pause(&self, point: &PausePoint, _: &PauseSnapshot) -> GateDecision {
        if matches!(point, PausePoint::AfterStep(_)) {
            GateDecision::AbortTest
        } else {
            GateDecision::Continue
        }
    }
}

#[tokio::test]
async fn gate_abort_preserves_completed_step_and_marks_verification_not_run() {
    let store = Arc::new(RecordingStore::default());
    let mut runner = runner(Some(store.clone())).await;
    runner.gate = Arc::new(AbortAfterStep);
    let mut def = echo_test();
    def.verify.stores.insert("postgres".into(), json!([]));
    let (result, execution) = runner
        .run_test_with_evidence(&def, &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Errored);
    assert_eq!(event(&execution, "step").status, ExecutionStatus::Passed);
    assert_eq!(event(&execution, "capture").status, ExecutionStatus::Passed);
    assert_eq!(
        event(&execution, "verify_store").status,
        ExecutionStatus::NotRun
    );
    assert_eq!(store.verifies.load(Ordering::SeqCst), 0);
    assert!(event(&execution, "finalization").details["error"]
        .as_str()
        .unwrap()
        .contains("aborted at step"));
}

#[tokio::test]
async fn transport_and_render_errors_preserve_started_attempts_without_fake_responses() {
    let mut runner = runner(None).await;
    runner.target_base_url = "http://127.0.0.1:0".into();
    let mut def = definition(json!({"test": "error", "steps": [
        {"name": "request", "request": {"method": "GET", "path": "/"}}
    ]}));
    let (result, execution) = runner
        .run_test_with_evidence(&def, &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Errored);
    let attempt = event(&execution, "http_attempt");
    assert_eq!(attempt.status, ExecutionStatus::Errored);
    assert!(attempt.details["request"].is_object());
    assert!(attempt.details.get("response").is_none());
    def.steps[0].request.path = Some("{{ ??? }}".into());
    let (result, execution) = runner
        .run_test_with_evidence(&def, &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Errored);
    let attempt = event(&execution, "http_attempt");
    assert_eq!(attempt.status, ExecutionStatus::Errored);
    assert!(
        attempt.details.get("request").is_none(),
        "a failed render never produced a prepared request"
    );
}

#[tokio::test]
async fn prepared_forms_queries_and_default_headers_match_the_sent_request() {
    let mut runner = runner(None).await;
    runner
        .defaults
        .request
        .headers
        .insert("X-Default".into(), "configured".into());
    let def = definition(json!({
        "test": "form",
        "mocks": {"target": {"stubs": [{"match": {"path": "/form"}, "response": {"status": 200}}]}},
        "verify": {"calls": [{"dependency": "target", "count": 1}]},
        "steps": [{"name": "form", "request": {
            "method": "POST", "path": "/target/form?duplicate=one",
            "query": {"duplicate": "two", "space": "hello world"},
            "form": {"field": "a & b"}, "headers": {"X-Default": "local"}
        }}]
    }));
    let (result, execution) = runner
        .run_test_with_evidence(&def, &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Passed);
    let request = &event(&execution, "http_attempt").details["request"];
    let call = &event(&execution, "finalization").details["recorded_calls"][0];
    assert_eq!(request["body"], call["request"]["body"]);
    assert_eq!(request["body"], "field=a+%26+b");
    assert_eq!(
        request["headers"]["x-default"],
        json!(["configured", "local"])
    );
    assert_eq!(request["query"].as_array().unwrap().len(), 3);
    assert_eq!(
        request["query"][0],
        json!({"name": "duplicate", "value": "one"})
    );
    assert_eq!(
        request["query"][1],
        json!({"name": "duplicate", "value": "two"})
    );
    assert_eq!(call["request"]["query"]["space"], "hello world");
}

#[tokio::test]
async fn source_only_reporting_captures_actual_credentials_without_trace_or_serialized_sources() {
    let runner = runner(None).await;
    let mut def = echo_test();
    def.vars.insert("credential".into(), json!("{{ uuid() }}"));
    def.steps[0].request.headers.insert(
        "Authorization".into(),
        json!("Bearer {{ vars.credential }}"),
    );
    def.steps[0]
        .request
        .query
        .insert("api_key".into(), json!("{{ vars.credential }}"));
    def.steps[0].request.json =
        Some(json!({"id": "{{ uuid() }}", "token": "{{ vars.credential }}"}));
    let (result, execution) = runner
        .run_test_for_reporting(&def, &IndexMap::new(), true, None, None, false)
        .await;
    assert_eq!(result.status, TestStatus::Passed);
    assert!(
        result.recorded_calls.is_empty(),
        "legacy success policy is preserved"
    );
    assert!(execution.events.is_empty());
    assert_eq!(execution.redaction_sources.len(), 4);
    let request = &execution.redaction_sources[0];
    let call = &execution.redaction_sources.last().unwrap()["request"];
    assert_eq!(request["body"], call["body_json"]);
    assert_eq!(
        request["headers"]["authorization"],
        call["headers"]["authorization"]
    );
    assert_eq!(
        request["headers"]["authorization"],
        format!("Bearer {}", request["body"]["token"].as_str().unwrap())
    );
    assert_eq!(request["query"][0]["value"], request["body"]["token"]);
    let serialized = serde_json::to_value(&execution).unwrap();
    assert!(serialized.get("redaction_sources").is_none());
    assert_eq!(serialized["events"], json!([]));
    let legacy = runner
        .run_test(&def, &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(legacy.status, TestStatus::Passed);
    assert!(legacy.recorded_calls.is_empty());
    assert!(serde_json::to_value(legacy)
        .unwrap()
        .get("redaction_sources")
        .is_none());
}

#[tokio::test]
async fn source_only_reporting_retains_mock_inputs_on_early_error_and_aligns_flow_stages() {
    let runner = runner(None).await;
    let mut def = echo_test();
    def.mocks["target"].stubs[0]
        .response
        .as_mut()
        .unwrap()
        .headers
        .insert("X-Api-Key".into(), "{{ request.body.id }}".into());
    def.steps[0].capture.insert(
        "token".into(),
        serde_json::from_value(json!({"jsonpath": "$.id"})).unwrap(),
    );
    def.steps[0].capture.insert(
        "missing".into(),
        serde_json::from_value(json!({"jsonpath": "$.absent"})).unwrap(),
    );
    let skipped = definition(json!({"test": "skipped", "skip": true}));
    let flow: FlowDef = serde_json::from_value(json!({"flow": "sources", "stages": [
        {"test": "skipped"}, {"test": "echo"}, {"test": "echo"}
    ]}))
    .unwrap();
    let tests = HashMap::from([(skipped.test.clone(), &skipped), (def.test.clone(), &def)]);
    let (outcome, executions) = runner.run_flow_for_reporting(&flow, &tests, false).await;
    assert_eq!(outcome.results.len(), 3);
    assert_eq!(executions.len(), 3);
    assert_eq!(outcome.results[1].status, TestStatus::Errored);
    assert_eq!(outcome.results[2].status, TestStatus::Skipped);
    assert!(outcome.results[1].captures.is_empty());
    assert_eq!(executions[1].redaction_sources.len(), 5);
    assert_eq!(
        executions[1].redaction_sources[0]["body"],
        executions[1].redaction_sources.last().unwrap()["request"]["body_json"]
    );
    let id = &executions[1].redaction_sources[0]["body"]["id"];
    assert_eq!(
        &executions[1].redaction_sources[1]["headers"]["x-api-key"],
        id
    );
    assert_eq!(&executions[1].redaction_sources[3]["token"], id);
    for (index, execution) in executions.iter().enumerate() {
        assert_eq!(execution.flow_stage, Some(index));
        assert!(execution.events.is_empty());
    }
}

#[tokio::test]
async fn source_only_response_headers_survive_body_timeout_without_response_details() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4096];
        assert!(socket.read(&mut buffer).await.unwrap() > 0);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Api-Key: issued-credential\r\n\r\n",
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    });
    let mut runner = runner(None).await;
    runner.target_base_url = format!("http://{address}");
    let def = definition(
        json!({"test": "partial sources", "timeout": "100ms", "steps": [
            {"name": "partial", "request": {"method": "GET", "path": "/"}}
        ]}),
    );
    let (result, execution) = runner
        .run_test_for_reporting(&def, &IndexMap::new(), true, None, None, false)
        .await;
    assert_eq!(result.status, TestStatus::Errored);
    assert!(execution.events.is_empty());
    assert_eq!(execution.redaction_sources.len(), 2);
    assert_eq!(
        execution.redaction_sources[1]["headers"]["x-api-key"],
        "issued-credential"
    );
    assert!(execution.redaction_sources[1].get("body").is_none());
    assert!(execution.redaction_sources[1].get("response").is_none());
    server.abort();
}
