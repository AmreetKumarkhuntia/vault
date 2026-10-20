use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use indexmap::IndexMap;
use serde_json::{json, Value};
use vault_dsl::{MockDep, UnmatchedPolicy};
use vault_mock::{ResponseRenderer, Session};

#[test]
fn times_budget_is_claimed_atomically() {
    let dep: MockDep = serde_json::from_value(json!({ "stubs": [
        { "match": { "path": "/x" }, "times": 1, "response": { "status": 200 } }
    ] }))
    .unwrap();
    let mut mocks = IndexMap::new();
    mocks.insert("d".to_string(), dep);
    let render: ResponseRenderer = Arc::new(|r: &Value, _| Ok(r.clone()));
    let session = Session::new("t", &mocks, UnmatchedPolicy::Fail, render);
    let stub = &session.deps["d"].stubs[0];
    let threads = 8;
    let barrier = &Barrier::new(threads);
    let overshoots = &AtomicU32::new(0);

    // Real threads released together by a barrier: a check-then-increment budget overshoots
    // within a few thousand rounds, an atomic claim never does.
    thread::scope(|scope| {
        for t in 0..threads {
            scope.spawn(move || {
                for _ in 0..3000 {
                    if t == 0 {
                        stub.served.store(0, Ordering::SeqCst);
                    }
                    barrier.wait();
                    stub.take_slot();
                    barrier.wait();
                    if t == 0 && stub.served.load(Ordering::SeqCst) > 1 {
                        overshoots.fetch_add(1, Ordering::SeqCst);
                    }
                    barrier.wait();
                }
            });
        }
    });
    assert_eq!(
        overshoots.load(Ordering::SeqCst),
        0,
        "a `times: 1` stub was claimed twice"
    );
}
