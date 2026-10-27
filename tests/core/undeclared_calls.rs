use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::json;
use vault_core::{NoopGate, TestRunner, TestStatus};
use vault_dsl::{Defaults, TestDef};
use vault_mock::MockServer;

/// The listener doubles as the target, so a step's request is the outbound call under test.
async fn runner() -> TestRunner {
    let mock = Arc::new(MockServer::start("127.0.0.1:0").await.unwrap());
    TestRunner {
        client: reqwest::Client::new(),
        target_base_url: mock.base_url(),
        defaults: Defaults::default(),
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

#[tokio::test]
async fn a_test_without_mocks_reports_the_targets_calls_without_failing() {
    let runner = runner().await;
    let stray = no_mocks(json!([{
        "name": "stray",
        "request": { "method": "GET", "path": "/euler/v1/offers" },
        "expect": { "status": 599 },
    }]));
    let (result, execution) = runner
        .run_test_with_evidence(&stray, &IndexMap::new(), true, None, None)
        .await;

    assert_eq!(result.status, TestStatus::Passed, "{:?}", result.verify);
    assert_eq!(result.recorded_calls.len(), 1);
    assert_eq!(
        result.recorded_calls[0]["request"]["path"],
        "/euler/v1/offers"
    );
    assert!(
        execution
            .events
            .iter()
            .all(|e| e.phase != "mock_quiet" && e.phase != "verify_calls"),
        "a test without mocks neither waits for settle nor verifies calls"
    );
}

#[tokio::test]
async fn a_quiet_test_without_mocks_records_nothing() {
    let runner = runner().await;
    let result = runner
        .run_test(&no_mocks(json!([])), &IndexMap::new(), true, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Passed, "{:?}", result.verify);
    assert!(result.recorded_calls.is_empty());
}
