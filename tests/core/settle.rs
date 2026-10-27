use std::sync::Arc;
use std::time::Instant;

use indexmap::IndexMap;
use vault_core::{NoopGate, TestRunner, TestStatus};
use vault_dsl::{parse_str, Defaults, TestDef};
use vault_mock::MockServer;

async fn runner() -> TestRunner {
    TestRunner {
        client: reqwest::Client::new(),
        target_base_url: "http://example.test".into(),
        defaults: Defaults::default(),
        global_seed: IndexMap::new(),
        stores: IndexMap::new(),
        mock: Arc::new(MockServer::start("127.0.0.1:0").await.unwrap()),
        gate: Arc::new(NoopGate),
        abort_run: std::sync::atomic::AtomicBool::new(false),
        shared_baseline: std::sync::atomic::AtomicBool::new(false),
        exclusive: tokio::sync::RwLock::new(()),
    }
}

fn test_def(raw: &str) -> TestDef {
    parse_str(raw, "test.test.yaml").unwrap()
}

fn settle_used(execution: &vault_core::TestExecution) -> u64 {
    execution
        .events
        .iter()
        .find(|e| e.phase == "mock_quiet")
        .expect("mock_quiet event")
        .details["settle_ms"]
        .as_u64()
        .unwrap()
}

#[tokio::test]
async fn test_level_settle_overrides_the_suite_default() {
    let runner = runner().await;

    let quick = test_def("test: quick\nmocks: { dep: {} }\nverify: { settle: 0ms }\n");
    let started = Instant::now();
    let (result, execution) = runner
        .run_test_with_evidence(&quick, &IndexMap::new(), false, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Passed);
    assert!(
        started.elapsed().as_millis() < 200,
        "settle 0ms still waited {:?}",
        started.elapsed()
    );
    assert_eq!(settle_used(&execution), 0);

    let default = test_def("test: default\nmocks: { dep: {} }\n");
    let started = Instant::now();
    let (result, execution) = runner
        .run_test_with_evidence(&default, &IndexMap::new(), false, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Passed);
    assert!(
        started.elapsed().as_millis() >= 500,
        "default settle returned after {:?}",
        started.elapsed()
    );
    assert_eq!(settle_used(&execution), 500);
}
