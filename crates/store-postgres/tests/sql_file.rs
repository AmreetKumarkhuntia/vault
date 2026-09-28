use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use vault_store::{DocMode, StoreConnConfig, StoreDriver};
use vault_store_postgres::PostgresDriver;

static TEMP_DIR_ID: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let id = TEMP_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vault-postgres-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temporary fixture directory should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sql_file/suite")
        .canonicalize()
        .expect("SQL fixture suite root should exist")
}

fn validate_at(doc: &Value, root: &Path) -> Result<(), String> {
    PostgresDriver
        .validate_with_suite_root(doc, DocMode::Seed, root)
        .map_err(|error| error.to_string())
}

fn assert_invalid_at(doc: Value, root: &Path, expected: &[&str]) -> String {
    let error = validate_at(&doc, root).expect_err("seed document should be rejected");
    for needle in expected {
        assert!(
            error.contains(needle),
            "expected `{needle}` in validation error:\n{error}"
        );
    }
    error
}

fn assert_invalid(doc: Value, expected: &[&str]) -> String {
    assert_invalid_at(doc, &fixture_root(), expected)
}

#[test]
fn accepts_each_well_formed_seed_entry_variant() {
    let doc = json!([
        {
            "table": "vault_sql_file_fixture_cases",
            "rows": [{ "id": 1, "label": "structured" }],
            "conflict": "error"
        },
        { "sql": "SELECT 1" },
        { "sql_file": "./fixtures/multi_statement.sql" }
    ]);

    validate_at(&doc, &fixture_root()).expect("valid seed entry variants should validate");
}

#[test]
fn legacy_validation_stays_filesystem_independent_but_checks_lexical_safety() {
    PostgresDriver
        .validate(
            &json!([{ "sql_file": "fixtures/not-present-yet.sql" }]),
            DocMode::Seed,
        )
        .expect("legacy validation must not require suite-root filesystem access");

    let error = PostgresDriver
        .validate(&json!([{ "sql_file": "../outside.sql" }]), DocMode::Seed)
        .expect_err("legacy validation should still reject unsafe lexical paths")
        .to_string();
    assert!(error.contains("seed.postgres[0].sql_file"), "{error}");
    assert!(error.contains("parent"), "{error}");
}

#[test]
fn rejects_malformed_seed_entries_and_mixed_variants() {
    let cases = [
        (json!({}), vec!["seed.postgres", "must be a list"]),
        (
            json!(["SELECT 1"]),
            vec!["seed.postgres[0]", "must be a mapping"],
        ),
        (
            json!([{ "unknown": true }]),
            vec!["seed.postgres[0]", "unknown key `unknown`"],
        ),
        (
            json!([{}]),
            vec![
                "seed.postgres[0]",
                "exactly one",
                "table",
                "sql",
                "sql_file",
            ],
        ),
        (
            json!([{ "table": "items", "sql": "SELECT 1", "rows": [] }]),
            vec!["seed.postgres[0]", "exactly one"],
        ),
        (
            json!([{ "sql": "SELECT 1", "sql_file": "fixtures/multi_statement.sql" }]),
            vec!["seed.postgres[0]", "exactly one"],
        ),
        (
            json!([{ "table": 42, "rows": [] }]),
            vec!["seed.postgres[0].table", "must be a string"],
        ),
        (
            json!([{ "table": "  ", "rows": [] }]),
            vec!["seed.postgres[0].table", "must not be empty"],
        ),
        (
            json!([{ "table": "items" }]),
            vec!["seed.postgres[0].rows", "must be a list"],
        ),
        (
            json!([{ "table": "items", "rows": {} }]),
            vec!["seed.postgres[0].rows", "must be a list"],
        ),
        (
            json!([{ "table": "items", "rows": [1] }]),
            vec!["seed.postgres[0].rows[0]", "must be a mapping"],
        ),
        (
            json!([{ "table": "items", "rows": [], "conflict": false }]),
            vec!["seed.postgres[0].conflict", "must be a string"],
        ),
        (
            json!([{ "sql": 42 }]),
            vec!["seed.postgres[0].sql", "must be a string"],
        ),
        (
            json!([{ "sql": " \n\t" }]),
            vec!["seed.postgres[0].sql", "must not be empty"],
        ),
        (
            json!([{ "sql_file": 42 }]),
            vec!["seed.postgres[0].sql_file", "must be a string"],
        ),
        (
            json!([{ "sql_file": "  " }]),
            vec!["seed.postgres[0].sql_file", "must not be empty"],
        ),
        (
            json!([{ "sql": "SELECT 1", "rows": [] }]),
            vec!["seed.postgres[0].rows", "only valid with `table`"],
        ),
        (
            json!([{ "sql_file": "fixtures/multi_statement.sql", "conflict": "ignore" }]),
            vec!["seed.postgres[0].conflict", "only valid with `table`"],
        ),
    ];

    for (doc, expected) in cases {
        assert_invalid(doc, &expected);
    }
}

#[test]
fn resolves_only_static_sql_files_contained_by_the_suite_root() {
    let root = fixture_root();
    let absolute = root.join("fixtures/multi_statement.sql");
    let cases = [
        (
            json!([{ "sql_file": "fixtures/missing.sql" }]),
            vec![
                "seed.postgres[0].sql_file",
                "fixtures/missing.sql",
                "could not resolve SQL file",
            ],
        ),
        (
            json!([{ "sql_file": absolute }]),
            vec!["seed.postgres[0].sql_file", "relative to the suite root"],
        ),
        (
            json!([{ "sql_file": "../outside.sql" }]),
            vec!["seed.postgres[0].sql_file", "parent"],
        ),
        (
            json!([{ "sql_file": "fixtures/not_sql.txt" }]),
            vec!["seed.postgres[0].sql_file", "`.sql` extension"],
        ),
        (
            json!([{ "sql_file": "fixtures/MIGRATION.SQL" }]),
            vec!["seed.postgres[0].sql_file", "`.sql` extension"],
        ),
        (
            json!([{ "sql_file": "fixtures/{{ vars.fixture }}.sql" }]),
            vec!["seed.postgres[0].sql_file", "static path", "template"],
        ),
        (
            json!([{ "sql_file": "fixtures" }]),
            vec!["seed.postgres[0].sql_file", "`.sql` extension"],
        ),
    ];

    for (doc, expected) in cases {
        assert_invalid_at(doc, &root, &expected);
    }

    let temp = TempDir::new("directory-with-sql-extension");
    let suite = temp.path().join("suite");
    fs::create_dir_all(suite.join("fixtures/a-directory.sql")).unwrap();
    assert_invalid_at(
        json!([{ "sql_file": "fixtures/a-directory.sql" }]),
        &suite,
        &["seed.postgres[0].sql_file", "regular file"],
    );
}

#[cfg(unix)]
#[test]
fn allows_internal_symlinks_and_rejects_suite_escaping_symlinks() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new("symlinks");
    let suite = temp.path().join("suite");
    let fixtures = suite.join("fixtures");
    fs::create_dir_all(&fixtures).unwrap();

    let target = fixtures.join("target.sql");
    fs::write(&target, "SELECT 1;").unwrap();
    symlink("target.sql", fixtures.join("internal.sql")).unwrap();
    validate_at(&json!([{ "sql_file": "fixtures/internal.sql" }]), &suite)
        .expect("a symlink whose canonical target stays inside the suite is safe");

    let outside = temp.path().join("outside.sql");
    fs::write(&outside, "SELECT 1;").unwrap();
    symlink(&outside, fixtures.join("escape.sql")).unwrap();
    let outside_canonical = outside.canonicalize().unwrap();
    assert_invalid_at(
        json!([{ "sql_file": "fixtures/escape.sql" }]),
        &suite,
        &[
            "seed.postgres[0].sql_file",
            "fixtures/escape.sql",
            &outside_canonical.display().to_string(),
            "outside the suite root",
        ],
    );
}

#[test]
fn file_read_errors_name_the_logical_and_resolved_paths() {
    let temp = TempDir::new("invalid-utf8");
    let suite = temp.path().join("suite");
    let fixtures = suite.join("fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    let logical = "fixtures/invalid-utf8.sql";
    let fixture = suite.join(logical);
    fs::write(&fixture, [0xff, 0xfe, 0xfd]).unwrap();
    let resolved = fixture.canonicalize().unwrap();

    assert_invalid_at(
        json!([{ "sql_file": logical }]),
        &suite,
        &[
            "seed.postgres[0].sql_file",
            logical,
            &resolved.display().to_string(),
            "UTF-8",
        ],
    );
}

#[test]
fn rejects_top_level_transaction_control_in_inline_sql_and_files() {
    let cases = [
        ("BEGIN", "BEGIN"),
        ("START /* trivia */ TRANSACTION", "START TRANSACTION"),
        ("COMMIT AND CHAIN", "COMMIT"),
        ("COMMIT PREPARED 'tx-id'", "COMMIT"),
        ("; -- leading trivia\nSELECT 1; cOmMiT", "COMMIT"),
        ("END", "END"),
        ("ROLLBACK TO SAVEPOINT earlier", "ROLLBACK"),
        ("ROLLBACK PREPARED 'tx-id'", "ROLLBACK"),
        ("ABORT", "ABORT"),
        ("SAVEPOINT nested", "SAVEPOINT"),
        ("RELEASE SAVEPOINT nested", "RELEASE"),
        ("RELEASE nested", "RELEASE"),
        ("PREPARE TRANSACTION 'tx-id'", "PREPARE TRANSACTION"),
        ("SET TRANSACTION READ ONLY", "SET TRANSACTION"),
        (
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
            "SET SESSION CHARACTERISTICS AS TRANSACTION",
        ),
    ];

    for (sql, command) in cases {
        assert_invalid(
            json!([{ "sql": sql }]),
            &["seed.postgres[0].sql", "transaction-control", command],
        );
    }

    let logical = "fixtures/transaction_control.sql";
    let root = fixture_root();
    let resolved = root.join(logical).canonicalize().unwrap();
    assert_invalid_at(
        json!([{ "sql_file": logical }]),
        &root,
        &[
            "seed.postgres[0].sql_file",
            logical,
            &resolved.display().to_string(),
            "transaction-control",
            "COMMIT",
        ],
    );
}

#[test]
fn transaction_keywords_in_literals_identifiers_comments_and_dollar_bodies_are_data() {
    let inline = r#"
SELECT 'COMMIT; it''s still data; ROLLBACK;';
SELECT E'escaped quote: \'COMMIT\'';
SELECT U&'BEGIN\\0041';
SELECT $$COMMIT; ROLLBACK;$$;
SELECT $MiXeD$BEGIN; $different$ is text; END;$MiXeD$;
SELECT 1 AS "COMMIT", 2 AS "SAVE""POINT";
SELECT 1 AS commitment, 2 AS rollback_count;
SELECT $1;
PREPARE safe_query (integer) AS SELECT $1;
SET LOCAL application_name = 'COMMIT';
-- COMMIT; BEGIN; ROLLBACK;\r
SELECT 1;
/* BEGIN; nested /* SAVEPOINT hidden; */ RELEASE; END; */
DO $vault_body$
BEGIN
    RAISE NOTICE 'COMMIT; ROLLBACK;';
END
$vault_body$;
"#;

    validate_at(
        &json!([
            { "sql": inline },
            { "sql_file": "fixtures/transaction_keywords_as_data.sql" }
        ]),
        &fixture_root(),
    )
    .expect("transaction keywords in protected regions must not be rejected");
}

fn store_config(alias: &str, url: String, suite_root: &Path) -> StoreConnConfig {
    StoreConnConfig {
        alias: alias.into(),
        url,
        options: json!({ "suite_root": suite_root }),
    }
}

async fn reset_live_table(pool: &PgPool) {
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS vault_sql_file_fixture_cases;\
         CREATE TABLE vault_sql_file_fixture_cases (\
           id BIGINT PRIMARY KEY,\
           label TEXT NOT NULL\
         );",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn live_rows(pool: &PgPool) -> Vec<(i64, String)> {
    sqlx::query("SELECT id, label FROM vault_sql_file_fixture_cases ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get("id"), row.get("label")))
        .collect()
}

#[tokio::test]
#[ignore = "requires VAULT_PG_URL (CI provides the PostgreSQL service)"]
async fn live_sql_file_ordering_rollback_and_runtime_mutation_matrix() {
    let database_url = std::env::var("VAULT_PG_URL")
        .expect("VAULT_PG_URL must be set when running the ignored live fixture matrix");
    let root = fixture_root();
    let driver = PostgresDriver;
    let store = driver
        .connect(&store_config("postgres", database_url.clone(), &root))
        .await
        .expect("PostgreSQL fixture test should connect");
    let pool = PgPool::connect(&database_url).await.unwrap();

    reset_live_table(&pool).await;

    let success = json!([
        { "sql_file": "fixtures/multi_statement.sql" },
        {
            "table": "vault_sql_file_fixture_cases",
            "rows": [{ "id": 3, "label": "from-structured-row" }]
        },
        {
            "sql": "UPDATE vault_sql_file_fixture_cases SET label = 'from-inline-sql' WHERE id = 3"
        }
    ]);
    driver
        .validate_with_suite_root(&success, DocMode::Seed, &root)
        .expect("live fixture seed should validate");
    let receipt = store.seed(&success).await.expect("SQL fixture should run");
    assert_eq!(
        receipt.entries,
        [
            "postgres: executed fixtures/multi_statement.sql",
            "postgres: vault_sql_file_fixture_cases +1 rows",
            "postgres: executed inline sql",
        ],
        "seed receipt order should match declaration order"
    );
    assert_eq!(
        live_rows(&pool).await,
        [
            (1, "from-file-one".into()),
            (2, "from-file-two".into()),
            (3, "from-inline-sql".into()),
        ],
        "file, structured, and inline entries should share declaration order"
    );

    sqlx::query("TRUNCATE vault_sql_file_fixture_cases")
        .execute(&pool)
        .await
        .unwrap();
    let failing = json!([
        {
            "sql": "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (10, 'earlier-entry')"
        },
        { "sql_file": "fixtures/invalid.sql" }
    ]);
    driver
        .validate_with_suite_root(&failing, DocMode::Seed, &root)
        .expect("invalid SQL syntax is a runtime concern, not a path error");
    let error = store
        .seed(&failing)
        .await
        .expect_err("invalid fixture SQL should fail the seed transaction")
        .to_string();
    let resolved = root.join("fixtures/invalid.sql").canonicalize().unwrap();
    for needle in [
        "seed.postgres[1].sql_file",
        "fixtures/invalid.sql",
        &resolved.display().to_string(),
    ] {
        assert!(error.contains(needle), "expected `{needle}` in:\n{error}");
    }
    assert!(
        live_rows(&pool).await.is_empty(),
        "all earlier entries and statements from the bad file must roll back"
    );

    let mutation_temp = TempDir::new("runtime-transaction-control");
    let mutation_root = mutation_temp.path().join("suite");
    let mutation_fixtures = mutation_root.join("fixtures");
    fs::create_dir_all(&mutation_fixtures).unwrap();
    let mutation_logical = "fixtures/mutable.sql";
    let mutation_path = mutation_root.join(mutation_logical);
    fs::write(
        &mutation_path,
        "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (30, 'initially-safe');",
    )
    .unwrap();
    let mutation_doc = json!([{ "sql_file": mutation_logical }]);
    driver
        .validate_with_suite_root(&mutation_doc, DocMode::Seed, &mutation_root)
        .expect("the initial fixture should pass static validation");
    let mutation_store = driver
        .connect(&store_config(
            "postgres",
            database_url.clone(),
            &mutation_root,
        ))
        .await
        .unwrap();
    fs::write(
        &mutation_path,
        "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (30, 'must-not-commit'); COMMIT; THIS IS INVALID SQL;",
    )
    .unwrap();
    let resolved_mutation = mutation_path.canonicalize().unwrap();
    let mutation_error = mutation_store
        .seed(&mutation_doc)
        .await
        .expect_err("runtime validation must reject a file changed to commit")
        .to_string();
    for needle in [
        "seed.postgres[0].sql_file",
        mutation_logical,
        &resolved_mutation.display().to_string(),
        "COMMIT",
    ] {
        assert!(
            mutation_error.contains(needle),
            "expected `{needle}` in:\n{mutation_error}"
        );
    }
    assert!(
        live_rows(&pool).await.is_empty(),
        "runtime transaction-control rejection must happen before execution"
    );

    sqlx::query("DROP TABLE vault_sql_file_fixture_cases")
        .execute(&pool)
        .await
        .unwrap();
}
