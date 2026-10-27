use std::sync::Arc;

use serde_json::{json, Value};
use sqlx::PgPool;
use vault_store::CheckResult;
use vault_store::{DocMode, StateStore, StoreConnConfig, StoreDriver, VerifyOpts};
use vault_store_postgres::PostgresDriver;

fn validate(doc: Value) -> bool {
    PostgresDriver.validate(&doc, DocMode::Verify).is_ok()
}

#[test]
fn verify_blocks_that_could_check_nothing_are_rejected_statically() {
    // `expect` as a mapping used to be skipped silently and the test passed.
    assert!(!validate(
        json!([{ "table": "orders", "expect": { "id": 1 } }])
    ));
    assert!(!validate(
        json!([{ "table": "orders", "count": "exactly", "expect": [{ "id": 1 }] }])
    ));
    assert!(!validate(
        json!([{ "table": "orders", "expect_absent": [{ "status": { "regex": "(oops" } }] }])
    ));
    assert!(!validate(
        json!([{ "table": "orders", "expect": [{ "created_at": { "$tag": "not_null" } }] }])
    ));
    assert!(validate(json!([{
        "table": "orders",
        "count": "exact",
        "expect": [{ "id": "{{ order_id }}", "status": { "regex": "^{{ vars.s }}$" } }]
    }])));
}

// The reset test truncates every table in public; live tests must not overlap.
static LIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn live_url() -> String {
    std::env::var("VAULT_PG_URL")
        .expect("VAULT_PG_URL must be set for the ignored live verify tests")
}

async fn live_store(url: &str) -> Arc<dyn StateStore> {
    PostgresDriver
        .connect(&StoreConnConfig {
            alias: "postgres".into(),
            url: url.to_string(),
            options: json!({ "suite_root": std::env::temp_dir() }),
        })
        .await
        .expect("live PostgreSQL should connect")
}

#[tokio::test]
#[ignore = "requires VAULT_PG_URL (CI provides the PostgreSQL service)"]
async fn verify_errors_instead_of_silently_passing() {
    let _serial = LIVE.lock().await;
    let url = live_url();
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS vault_cap_cases;\
         CREATE TABLE vault_cap_cases (id BIGINT PRIMARY KEY);\
         INSERT INTO vault_cap_cases SELECT generate_series(1, 1001);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = live_store(&url).await;
    let opts = VerifyOpts::default();
    let absent_1001 = json!([{ "table": "vault_cap_cases", "expect_absent": [{ "id": 1001 }] }]);

    let err = store.verify(&absent_1001, &opts).await.unwrap_err();
    assert!(err.to_string().contains("more than 1000 rows"), "{err}");

    sqlx::raw_sql("DELETE FROM vault_cap_cases WHERE id = 1001")
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.verify(&absent_1001, &opts).await.unwrap().passed());

    // Shapes that validation cannot see (they may arrive through a template) still error.
    let mapping = json!([{ "table": "vault_cap_cases", "expect": { "id": 1 } }]);
    assert!(store.verify(&mapping, &opts).await.is_err());
    let bad_count = json!([{ "table": "vault_cap_cases", "count": 2, "expect": [{ "id": 1 }] }]);
    assert!(store.verify(&bad_count, &opts).await.is_err());
}

#[tokio::test]
#[ignore = "requires VAULT_PG_URL; TRUNCATEs every table in public, so use a throwaway database"]
async fn reset_keeps_migration_tables_even_with_a_custom_exclude() {
    let _serial = LIVE.lock().await;
    let url = live_url();
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS goose_db_version; CREATE TABLE goose_db_version (v INT);\
         INSERT INTO goose_db_version VALUES (1);\
         DROP TABLE IF EXISTS vault_reset_cases; CREATE TABLE vault_reset_cases (id INT);\
         INSERT INTO vault_reset_cases VALUES (1);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = live_store(&url).await;

    store
        .reset(&json!({ "exclude": ["vault_other"] }))
        .await
        .unwrap();
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM goose_db_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    let wiped: i64 = sqlx::query_scalar("SELECT count(*) FROM vault_reset_cases")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((kept, wiped), (1, 0));

    assert!(store.reset(&json!({ "mode": "delete" })).await.is_err());
}

#[tokio::test]
#[ignore = "requires VAULT_PG_URL (CI provides the PostgreSQL service)"]
async fn schema_qualified_and_mixed_case_tables_are_addressable() {
    let _serial = LIVE.lock().await;
    let url = live_url();
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql(
        "CREATE SCHEMA IF NOT EXISTS vault_t;\
         DROP TABLE IF EXISTS vault_t.cases; CREATE TABLE vault_t.cases (id INT);\
         DROP TABLE IF EXISTS \"Order\"; CREATE TABLE \"Order\" (id INT);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = live_store(&url).await;

    store
        .seed(&json!([
            { "table": "vault_t.cases", "rows": [{ "id": 1 }] },
            { "table": "Order", "rows": [{ "id": 2 }] }
        ]))
        .await
        .unwrap();
    let outcome = store
        .verify(
            &json!([
                { "table": "vault_t.cases", "expect": [{ "id": 1 }] },
                { "table": "Order", "expect": [{ "id": 2 }] }
            ]),
            &VerifyOpts::default(),
        )
        .await
        .unwrap();
    assert!(
        outcome.passed(),
        "{:?}",
        outcome.failures().collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "requires VAULT_PG_URL (CI provides the PostgreSQL service)"]
async fn scoped_watch_diffs_only_rows_matching_the_filter() {
    let _serial = LIVE.lock().await;
    let url = live_url();
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS vault_watch_scoped;\
         CREATE TABLE vault_watch_scoped (id INT PRIMARY KEY, shop TEXT);\
         INSERT INTO vault_watch_scoped VALUES (1, 'a'), (2, 'b');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = live_store(&url).await;
    let scoped = json!([{ "table": "vault_watch_scoped", "where": { "shop": "a" } }]);
    let whole = json!(["vault_watch_scoped"]);
    let before_scoped = store.snapshot(&scoped).await.unwrap();
    let before_whole = store.snapshot(&whole).await.unwrap();

    sqlx::raw_sql("INSERT INTO vault_watch_scoped VALUES (3, 'a'), (4, 'b')")
        .execute(&pool)
        .await
        .unwrap();

    let failures = |outcome: &vault_store::VerifyOutcome| {
        outcome
            .checks
            .iter()
            .filter(|c| matches!(c, CheckResult::Fail(_)))
            .count()
    };
    let scoped_diff = store.diff_snapshot(&scoped, &before_scoped).await.unwrap();
    let whole_diff = store.diff_snapshot(&whole, &before_whole).await.unwrap();
    assert_eq!(failures(&scoped_diff), 1, "{scoped_diff:?}");
    assert_eq!(failures(&whole_diff), 2, "{whole_diff:?}");
    assert!(store.snapshot(&json!([{ "where": {} }])).await.is_err());
}
