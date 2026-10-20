use serde_json::json;
use vault_store::{FailureKind, StoreConnConfig, StoreDriver, VerifyOpts};
use vault_store_redis::RedisDriver;

#[tokio::test]
#[ignore = "requires VAULT_REDIS_URL (CI provides the Redis service); flushes that logical db"]
async fn reset_typos_error_and_binary_values_do_not_break_ttl_checks() {
    let url = std::env::var("VAULT_REDIS_URL")
        .expect("VAULT_REDIS_URL must be set for the ignored live redis tests");
    let store = RedisDriver
        .connect(&StoreConnConfig {
            alias: "redis".into(),
            url: url.clone(),
            options: json!({}),
        })
        .await
        .expect("live Redis should connect");
    let mut c = redis::Client::open(url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();

    store.reset(&json!({})).await.unwrap();
    redis::cmd("SET")
        .arg("a:1")
        .arg("x")
        .query_async::<()>(&mut c)
        .await
        .unwrap();
    redis::cmd("SET")
        .arg("bin")
        .arg(&[0xffu8, 0xfe][..])
        .query_async::<()>(&mut c)
        .await
        .unwrap();

    // A typo in `mode` or a `scan` without prefixes must error, not flush or skip.
    assert!(store
        .reset(&json!({ "mode": "scna", "prefixes": ["a:"] }))
        .await
        .is_err());
    assert!(store.reset(&json!({ "mode": "scan" })).await.is_err());
    let still: bool = redis::cmd("EXISTS")
        .arg("a:1")
        .query_async(&mut c)
        .await
        .unwrap();
    assert!(still, "a typo in mode must not flush the db");

    let opts = VerifyOpts::default();
    let ttl_only = store
        .verify(&json!([{ "key": "bin", "ttl": { "lt": 0 } }]), &opts)
        .await
        .unwrap();
    assert!(
        ttl_only.passed(),
        "{:?}",
        ttl_only.failures().collect::<Vec<_>>()
    );
    let absent = store
        .verify(&json!([{ "key": "bin", "absent": true }]), &opts)
        .await
        .unwrap();
    assert!(matches!(
        absent.failures().next().map(|f| &f.kind),
        Some(FailureKind::UnexpectedKey { .. })
    ));
    assert!(store.verify(&json!([{ "key": 123 }]), &opts).await.is_err());
}
