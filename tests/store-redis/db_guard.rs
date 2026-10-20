use std::time::Duration;

use serde_json::{json, Value};
use vault_store::{StoreConnConfig, StoreDriver};
use vault_store_redis::RedisDriver;

fn cfg(url: &str, options: Value) -> StoreConnConfig {
    StoreConnConfig {
        alias: "redis".into(),
        url: url.into(),
        options,
    }
}

#[tokio::test]
async fn db_zero_is_refused_before_any_network_io() {
    for url in [
        "redis://127.0.0.1:1/0",
        "redis://127.0.0.1:1",
        "redis://127.0.0.1:1/0?protocol=resp3",
    ] {
        let Err(err) = RedisDriver.connect(&cfg(url, json!({}))).await else {
            panic!("{url}: db 0 must be refused");
        };
        assert!(err.to_string().contains("refusing db 0"), "{url}: {err}");
    }
}

#[tokio::test]
async fn non_zero_db_with_url_options_is_not_mistaken_for_db_zero() {
    // Port 1 is closed: a connection error or a timeout are the only acceptable outcomes,
    // never the db-0 refusal the old last-path-segment parser produced for `?protocol=`.
    for (url, options) in [
        ("redis://127.0.0.1:1/15?protocol=resp3", json!({})),
        ("redis://127.0.0.1:1/0", json!({ "allow_db0": true })),
    ] {
        let attempt = tokio::time::timeout(
            Duration::from_millis(200),
            RedisDriver.connect(&cfg(url, options)),
        )
        .await;
        if let Ok(Err(err)) = &attempt {
            assert!(!err.to_string().contains("refusing db 0"), "{url}: {err}");
        }
    }
}
