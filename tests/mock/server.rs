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
    let held = server
        .claim(vec![claim("/platform", vault_mock::ClaimMode::Provide)])
        .await;
    let _disjoint = server
        .claim(vec![claim("/euler", vault_mock::ClaimMode::Provide)])
        .await;

    let contender = {
        let server = server.clone();
        tokio::spawn(async move {
            server
                .claim(vec![claim("/platform/s", vault_mock::ClaimMode::Provide)])
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!contender.is_finished(), "an overlapping claim must wait");

    drop(held);
    tokio::time::timeout(Duration::from_secs(2), contender)
        .await
        .expect("the waiting claim proceeds once the holder releases")
        .unwrap();
}

fn claim(prefix: &str, mode: vault_mock::ClaimMode) -> vault_mock::Claim {
    vault_mock::Claim {
        prefix: prefix.into(),
        mode,
    }
}

#[tokio::test]
async fn forbids_share_a_prefix_and_claims_are_granted_in_arrival_order() {
    use std::time::Duration;
    use vault_mock::ClaimMode::{Forbid, Provide};
    use vault_mock::MockServer;

    let server = std::sync::Arc::new(MockServer::start("127.0.0.1:0").await.unwrap());
    let first = server.claim(vec![claim("/euler", Forbid)]).await;
    let second = tokio::time::timeout(
        Duration::from_millis(200),
        server.claim(vec![claim("/euler", Forbid)]),
    )
    .await
    .expect("two forbids share a prefix");

    let provider = {
        let server = server.clone();
        tokio::spawn(async move { server.claim(vec![claim("/euler", Provide)]).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!provider.is_finished(), "a provide waits for the forbids");

    let late_forbid = {
        let server = server.clone();
        tokio::spawn(async move { server.claim(vec![claim("/euler/v1", Forbid)]).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !late_forbid.is_finished(),
        "a forbid arriving after a waiting provide queues behind it"
    );

    drop(first);
    drop(second);
    let provider = tokio::time::timeout(Duration::from_secs(2), provider)
        .await
        .expect("the provide is granted once the forbids release")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!late_forbid.is_finished(), "the provide is exclusive");

    drop(provider);
    tokio::time::timeout(Duration::from_secs(2), late_forbid)
        .await
        .expect("the late forbid is granted after the provide")
        .unwrap();
}

#[tokio::test]
async fn a_cancelled_waiter_stops_blocking_the_claims_behind_it() {
    use std::time::Duration;
    use vault_mock::ClaimMode::{Forbid, Provide};
    use vault_mock::MockServer;

    let server = std::sync::Arc::new(MockServer::start("127.0.0.1:0").await.unwrap());
    let held = server.claim(vec![claim("/euler", Forbid)]).await;
    let provider = {
        let server = server.clone();
        tokio::spawn(async move { server.claim(vec![claim("/euler", Provide)]).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let forbid = {
        let server = server.clone();
        tokio::spawn(async move { server.claim(vec![claim("/euler", Forbid)]).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!forbid.is_finished());

    provider.abort();
    tokio::time::timeout(Duration::from_secs(2), forbid)
        .await
        .expect("the forbid proceeds once the waiting provide is gone")
        .unwrap();
    drop(held);
}

#[test]
fn claim_mode_follows_stubs_and_the_resolved_unmatched_policy() {
    use vault_mock::claim_mode;
    use vault_mock::ClaimMode::{Forbid, Provide};
    let dep = |v: Value| serde_json::from_value::<MockDep>(v).unwrap();
    assert_eq!(claim_mode(&dep(json!({})), &UnmatchedPolicy::Fail), Forbid);
    assert_eq!(
        claim_mode(&dep(json!({})), &UnmatchedPolicy::Allow),
        Provide
    );
    assert_eq!(
        claim_mode(&dep(json!({"unmatched": "fail"})), &UnmatchedPolicy::Allow),
        Forbid
    );
    assert_eq!(
        claim_mode(
            &dep(json!({"stubs": [{"match": {"path": "/x"}, "response": {"status": 200}}]})),
            &UnmatchedPolicy::Fail
        ),
        Provide
    );
}
