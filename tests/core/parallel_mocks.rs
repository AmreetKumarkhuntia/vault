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

fn forbid_test(name: &str, settle: &str) -> TestDef {
    serde_json::from_value(json!({
        "test": name,
        "verify": { "no_calls": ["euler"], "settle": settle },
    }))
    .unwrap()
}

#[tokio::test]
async fn tests_forbidding_the_same_dependency_overlap_in_either_spelling() {
    let runner = runner().await;
    let empty = IndexMap::new();
    let explicit = forbid_test("explicit", "500ms");
    let empty_mount: TestDef = serde_json::from_value(json!({
        "test": "empty mount",
        "mocks": { "euler": {} },
        "verify": { "settle": "500ms" },
    }))
    .unwrap();

    let started = std::time::Instant::now();
    let (ra, rb) = tokio::join!(
        runner.run_test(&explicit, &empty, false, None, None),
        runner.run_test(&empty_mount, &empty, false, None, None),
    );

    assert_eq!(ra.status, TestStatus::Passed, "{:?}", ra.verify);
    assert_eq!(rb.status, TestStatus::Passed, "{:?}", rb.verify);
    // Each waits out its own 500 ms settle; overlapping, the pair finishes well under
    // the full second a serialized pair needs.
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "forbids serialized: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_forbidden_call_fails_every_test_forbidding_it() {
    let runner = runner().await;
    let empty = IndexMap::new();
    let a = forbid_test("a", "1s");
    let b = forbid_test("b", "1s");
    let caller: TestDef = serde_json::from_value(json!({
        "test": "caller",
        "mocks": { "platform": { "stubs": [
            { "match": { "path": "/x" }, "response": { "status": 200 } }
        ] } },
        "steps": [{
            "name": "stray",
            "request": { "method": "GET", "path": "/euler/v1/offers" },
            "expect": { "status": 599 },
        }],
        "verify": { "settle": "0ms", "calls": [{ "dependency": "platform", "count": 0 }] },
    }))
    .unwrap();

    let (ra, rb, rc) = tokio::join!(
        runner.run_test(&a, &empty, false, None, None),
        runner.run_test(&b, &empty, false, None, None),
        async {
            // Both forbids are armed and waiting out their settle when the call lands.
            tokio::time::sleep(Duration::from_millis(200)).await;
            runner.run_test(&caller, &empty, false, None, None).await
        },
    );

    assert_eq!(rc.status, TestStatus::Passed, "{:?}", rc.verify);
    for result in [&ra, &rb] {
        assert_eq!(result.status, TestStatus::Failed);
        let verify = format!("{:?}", result.verify);
        assert!(verify.contains("`euler` must not be called"), "{verify}");
        assert!(result.recorded_calls.iter().any(|call| {
            call["dependency"] == "euler" && call["request"]["path"] == "/v1/offers"
        }));
    }
}

#[tokio::test]
async fn above_one_lane_an_unclaimed_call_goes_to_the_orphan_log_and_blames_nobody() {
    let runner = runner().await;
    runner.mock.set_parallel(true);
    let empty = IndexMap::new();
    let mocker: TestDef = serde_json::from_value(json!({
        "test": "mocker",
        "mocks": { "platform": { "stubs": [
            { "match": { "path": "/x" }, "response": { "status": 200 } }
        ] } },
        "verify": { "settle": "1s" },
    }))
    .unwrap();
    let caller: TestDef = serde_json::from_value(json!({
        "test": "caller",
        "steps": [{
            "name": "stray",
            "request": { "method": "GET", "path": "/nobody/x" },
            "expect": { "status": 599 },
        }],
    }))
    .unwrap();

    let (rm, rc) = tokio::join!(runner.run_test(&mocker, &empty, false, None, None), async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        runner.run_test(&caller, &empty, false, None, None).await
    },);

    assert_eq!(rm.status, TestStatus::Passed, "{:?}", rm.verify);
    assert_eq!(rc.status, TestStatus::Passed, "{:?}", rc.verify);
    assert!(
        rc.recorded_calls.is_empty(),
        "nobody is blamed, not even the caller"
    );
    let orphans = runner.mock.drain_orphans();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].path, "/nobody/x");
    let names: Vec<&str> = orphans[0]
        .candidates
        .iter()
        .map(|c| c.test.as_str())
        .collect();
    assert!(
        names.contains(&"mocker") && names.contains(&"caller"),
        "{names:?}"
    );
    assert!(
        runner.mock.drain_orphans().is_empty(),
        "draining empties the log"
    );
}

#[tokio::test]
async fn a_mock_test_calling_out_beside_a_no_mock_test_is_blamed_only_by_its_serial_rerun() {
    let runner = runner().await;
    runner.mock.set_parallel(true);
    let empty = IndexMap::new();
    // Keeps the no-mock test armed: its request is answered by this stub after a delay.
    let slowpoke: TestDef = serde_json::from_value(json!({
        "test": "slowpoke",
        "mocks": { "platform": { "stubs": [
            { "match": { "path": "/slow" }, "response": { "status": 200, "latency": "1s" } }
        ] } },
        "verify": { "settle": "1500ms", "unexpected": "allow" },
    }))
    .unwrap();
    let nomock: TestDef = serde_json::from_value(json!({
        "test": "nomock",
        "steps": [{
            "name": "slow",
            "request": { "method": "GET", "path": "/platform/slow" },
            "expect": { "status": [200, 599] },
        }],
    }))
    .unwrap();
    let mocker: TestDef = serde_json::from_value(json!({
        "test": "mocker",
        "mocks": { "billing": { "stubs": [
            { "match": { "path": "/x" }, "response": { "status": 200 } }
        ] } },
        "steps": [{
            "name": "stray",
            "request": { "method": "GET", "path": "/nobody/x" },
            "expect": { "status": 599 },
        }],
        "verify": { "settle": "0ms" },
    }))
    .unwrap();

    let (rs, rn, rm) = tokio::join!(
        runner.run_test(&slowpoke, &empty, false, None, None),
        async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            runner.run_test(&nomock, &empty, false, None, None).await
        },
        async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            runner.run_test(&mocker, &empty, false, None, None).await
        },
    );
    assert_eq!(rs.status, TestStatus::Passed, "{:?}", rs.verify);
    assert_eq!(rn.status, TestStatus::Passed, "{:?}", rn.verify);
    assert_eq!(
        rm.status,
        TestStatus::Passed,
        "in parallel nobody is blamed: {:?}",
        rm.verify
    );
    let orphans = runner.mock.drain_orphans();
    assert_eq!(orphans.len(), 1, "{orphans:?}");
    let names: Vec<&str> = orphans[0]
        .candidates
        .iter()
        .map(|c| c.test.as_str())
        .collect();
    assert!(
        names.contains(&"nomock") && names.contains(&"mocker"),
        "{names:?}"
    );

    // The serial re-run settles it: the call lands in the only armed session.
    runner.mock.set_parallel(false);
    let rm = runner.run_test(&mocker, &empty, false, None, None).await;
    assert_eq!(rm.status, TestStatus::Failed);
    assert!(
        format!("{:?}", rm.verify).contains("<unknown>"),
        "{:?}",
        rm.verify
    );
    let rn = runner.run_test(&nomock, &empty, false, None, None).await;
    assert_eq!(rn.status, TestStatus::Passed, "{:?}", rn.verify);
    assert_eq!(rn.recorded_calls.len(), 1);
}

#[tokio::test]
async fn two_forbidden_mounts_under_different_names_are_both_enforced() {
    let runner = runner().await;
    let test: TestDef = serde_json::from_value(json!({
        "test": "two forbids",
        "steps": [
            { "name": "s", "request": { "method": "GET", "path": "/platform/s/x" }, "expect": { "status": 599 } },
            { "name": "w", "request": { "method": "GET", "path": "/platform/w/y" }, "expect": { "status": 599 } },
        ],
        "verify": { "settle": "0ms", "no_calls": [
            { "dependency": "platform_s", "prefix": "/platform/s" },
            { "dependency": "platform_w", "prefix": "/platform/w" },
        ] },
    }))
    .unwrap();
    let result = runner
        .run_test(&test, &IndexMap::new(), false, None, None)
        .await;
    assert_eq!(result.status, TestStatus::Failed);
    let verify = format!("{:?}", result.verify);
    assert!(
        verify.contains("`platform_s` must not be called"),
        "{verify}"
    );
    assert!(
        verify.contains("`platform_w` must not be called"),
        "{verify}"
    );
}
