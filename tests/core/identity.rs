use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::json;
use vault_core::{NoopGate, TestRunner, TestStatus};
use vault_dsl::{Defaults, FlowDef, TestDef};
use vault_mock::MockServer;

/// The mock echoes the default header back, so what the target received is observable.
async fn runner(header: &str) -> TestRunner {
    let mock = Arc::new(MockServer::start("127.0.0.1:0").await.unwrap());
    let mut defaults = Defaults::default();
    defaults
        .request
        .headers
        .insert("X-Flow".into(), header.into());
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

fn echo_test(name: &str) -> TestDef {
    serde_json::from_value(json!({
        "test": name,
        "mocks": { "d": { "stubs": [{
            "match": { "path": "/x" },
            "response": { "status": 200, "json": { "echo": "{{ request.headers['x-flow'] }}" } }
        }] } },
        "steps": [{
            "name": "call",
            "request": { "method": "GET", "path": "/d/x" },
            "expect": { "status": 200 },
            "capture": { "echo": { "jsonpath": "$.echo" } },
        }],
        "verify": { "settle": "0ms", "calls": [{ "dependency": "d", "count": 1 }] },
    }))
    .unwrap()
}

fn is_uid(s: &str) -> bool {
    s.len() == 8 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[tokio::test]
async fn every_stage_of_a_flow_shares_a_stable_uid_in_rendered_default_headers() {
    let runner = runner("{{ flow.name }}:{{ flow.uid }}").await;
    let test = echo_test("echo");
    let tests: HashMap<String, &TestDef> = HashMap::from([("echo".to_string(), &test)]);
    let flow: FlowDef = serde_json::from_value(json!({
        "flow": "checkout",
        "stages": [{ "test": "echo" }, { "test": "echo" }],
    }))
    .unwrap();

    let echoes = |outcome: vault_core::FlowOutcome| -> Vec<String> {
        outcome
            .results
            .iter()
            .map(|r| {
                assert_eq!(r.status, TestStatus::Passed, "{:?}", r.verify);
                r.captures["echo"].as_str().unwrap().to_string()
            })
            .collect()
    };
    let first = echoes(runner.run_flow(&flow, &tests).await);
    let again = echoes(runner.run_flow(&flow, &tests).await);

    assert_eq!(first[0], first[1]);
    assert_eq!(first, again);
    let (name, uid) = first[0].split_once(':').unwrap();
    assert_eq!(name, "checkout");
    assert!(is_uid(uid), "{uid}");
}

#[tokio::test]
async fn standalone_tests_get_a_uid_from_their_name() {
    let runner = runner("{{ test.uid }}").await;
    let empty = IndexMap::new();
    let a = runner
        .run_test(&echo_test("a"), &empty, false, None, None)
        .await;
    let b = runner
        .run_test(&echo_test("b"), &empty, false, None, None)
        .await;
    assert_eq!(a.status, TestStatus::Passed, "{:?}", a.verify);
    assert_eq!(b.status, TestStatus::Passed, "{:?}", b.verify);
    let (a, b) = (
        a.captures["echo"].as_str().unwrap(),
        b.captures["echo"].as_str().unwrap(),
    );
    assert!(is_uid(a) && is_uid(b), "{a} {b}");
    assert_ne!(a, b);
}
