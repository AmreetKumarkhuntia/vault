use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::json;
use vault_core::{NoopGate, TestRunner, TestStatus};
use vault_dsl::{Defaults, TestDef, UnmatchedPolicy};
use vault_mock::MockServer;

/// The listener doubles as the target, so a step's request is the outbound call under test.
async fn runner(unmatched: UnmatchedPolicy) -> TestRunner {
    let mock = Arc::new(MockServer::start("127.0.0.1:0").await.unwrap());
    let mut defaults = Defaults::default();
    defaults.mock.unmatched = unmatched;
    TestRunner {
        client: reqwest::Client::new(),
        target_base_url: mock.base_url(),
        defaults,
        global_seed: IndexMap::new(),
        stores: IndexMap::new(),
        mock,
        gate: Arc::new(NoopGate),
        abort_run: std::sync::atomic::AtomicBool::new(false),
        shared_baseline: std::sync::atomic::AtomicBool::new(false),
        exclusive: tokio::sync::RwLock::new(()),
    }
}

fn no_mocks(steps: serde_json::Value) -> TestDef {
    serde_json::from_value(json!({ "test": "no mocks", "steps": steps })).unwrap()
}

fn stray_call() -> serde_json::Value {
    json!([{
        "name": "stray",
        "request": { "method": "GET", "path": "/euler/v1/offers" },
        "expect": { "status": 599 },
    }])
}

#[tokio::test]
async fn a_test_without_mocks_fails_when_the_target_calls_out() {
    let runner = runner(UnmatchedPolicy::Fail).await;
    let (result, execution) = runner
        .run_test_with_evidence(&no_mocks(stray_call()), &IndexMap::new(), true, None, None)
        .await;

    assert_eq!(result.status, TestStatus::Failed);
    let verify = format!("{:?}", result.verify);
    assert!(verify.contains("undeclared outbound call"), "{verify}");
    assert!(verify.contains("GET /euler/v1/offers"), "{verify}");
    assert_eq!(result.recorded_calls.len(), 1);
    assert!(
        execution.events.iter().all(|e| e.phase != "mock_quiet"),
        "a test without mocks must not wait for settle"
    );
}

#[tokio::test]
async fn allow_keeps_stray_calls_silent_and_quiet_tests_are_unaffected() {
    let lenient = runner(UnmatchedPolicy::Allow).await;
    let result = lenient
        .run_test(&no_mocks(stray_call()), &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Passed, "{:?}", result.verify);

    let strict = runner(UnmatchedPolicy::Fail).await;
    let (result, execution) = strict
        .run_test_with_evidence(&no_mocks(json!([])), &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Passed, "{:?}", result.verify);
    assert!(execution.events.iter().all(|e| e.phase != "verify_calls"));
}
