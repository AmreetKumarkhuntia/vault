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

#[test]
fn mounts_overlap_at_segment_boundaries_only() {
    use vault_mock::mounts_overlap;
    assert!(mounts_overlap("/platform", "/platform/s"));
    assert!(mounts_overlap("/platform/s", "/platform"));
    assert!(mounts_overlap("/euler", "/euler"));
    assert!(mounts_overlap("", "/anything"));
    assert!(!mounts_overlap("/platform/s", "/platform/arch-store"));
    assert!(!mounts_overlap("/a", "/ab"));
}

#[tokio::test]
async fn conflicting_claim_waits_for_the_holder_and_disjoint_claims_do_not() {
    use std::time::Duration;
    use vault_mock::MockServer;

    let server = std::sync::Arc::new(MockServer::start("127.0.0.1:0").await.unwrap());
    let held = server.claim(vec!["/platform".into()]).await;
    let _disjoint = server.claim(vec!["/euler".into()]).await;

    let contender = {
        let server = server.clone();
        tokio::spawn(async move { server.claim(vec!["/platform/s".into()]).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!contender.is_finished(), "an overlapping claim must wait");

    drop(held);
    tokio::time::timeout(Duration::from_secs(2), contender)
        .await
        .expect("the waiting claim proceeds once the holder releases")
        .unwrap();
}
