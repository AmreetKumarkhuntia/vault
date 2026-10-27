use std::sync::Arc;
use std::time::Duration;

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
        shared_baseline: std::sync::atomic::AtomicBool::new(true),
        exclusive: tokio::sync::RwLock::new(()),
    }
}

fn mock_test(name: &str, dep: &str, answer: &str) -> TestDef {
    serde_json::from_value(json!({
        "test": name,
        "mocks": { dep: { "stubs": [
            { "match": { "path": "/x" }, "response": { "status": 200, "json": { "from": answer } } }
        ] } },
        "steps": [{
            "name": "call",
            "request": { "method": "GET", "path": format!("/{dep}/x") },
            "expect": { "status": 200, "json_partial": { "from": answer } },
        }],
        "verify": { "settle": "0ms", "calls": [{ "dependency": dep, "count": 1 }] },
    }))
    .unwrap()
}

#[tokio::test]
async fn lanes_with_different_prefixes_each_see_only_their_own_calls() {
    let runner = runner().await;
    let empty = IndexMap::new();
    let a = mock_test("a", "a", "a");
    let b = mock_test("b", "b", "b");

    let (ra, rb) = tokio::join!(
        runner.run_test(&a, &empty, false, None, None),
        runner.run_test(&b, &empty, false, None, None),
    );

    assert_eq!(ra.status, TestStatus::Passed, "{:?}", ra.verify);
    assert_eq!(rb.status, TestStatus::Passed, "{:?}", rb.verify);
}

#[tokio::test]
async fn tests_sharing_a_prefix_run_one_after_the_other() {
    let runner = runner().await;
    let empty = IndexMap::new();
    let first = mock_test("first", "shared", "first");
    let second = mock_test("second", "shared", "second");

    let (r1, r2) = tokio::join!(
        runner.run_test(&first, &empty, false, None, None),
        runner.run_test(&second, &empty, false, None, None),
    );

    // Each step asserts its own stub's answer, so a pass proves the second waited.
    assert_eq!(r1.status, TestStatus::Passed, "{:?}", r1.verify);
    assert_eq!(r2.status, TestStatus::Passed, "{:?}", r2.verify);
}

#[tokio::test]
async fn a_call_to_an_unmocked_prefix_fails_every_running_test() {
    let runner = runner().await;
    let empty = IndexMap::new();
    let caller: TestDef = serde_json::from_value(json!({
        "test": "caller",
        "mocks": { "a": { "stubs": [] } },
        "steps": [{
            "name": "stray",
            "request": { "method": "GET", "path": "/nobody/x" },
            "expect": { "status": 599 },
        }],
        "verify": { "settle": "0ms" },
    }))
    .unwrap();
    let bystander: TestDef = serde_json::from_value(json!({
        "test": "bystander",
        "mocks": { "b": { "stubs": [] } },
        "verify": { "settle": "1s" },
    }))
    .unwrap();

    let (rb, rc) = tokio::join!(
        runner.run_test(&bystander, &empty, false, None, None),
        async {
            // The bystander is armed and waiting out its settle when the stray call lands.
            tokio::time::sleep(Duration::from_millis(200)).await;
            runner.run_test(&caller, &empty, false, None, None).await
        },
    );

    assert_eq!(rc.status, TestStatus::Failed);
    assert_eq!(rb.status, TestStatus::Failed);
    for result in [&rb, &rc] {
        assert!(
            result
                .recorded_calls
                .iter()
                .any(|call| call["dependency"] == "<unknown>"),
            "{:?}",
            result.recorded_calls
        );
    }
}
