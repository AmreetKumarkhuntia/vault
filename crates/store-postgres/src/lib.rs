//! Postgres driver: seeding, TRUNCATE isolation, and expectation-driven
//! verification with near-miss reporting.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use vault_store::matchers::{self, MatchCtx};
use vault_store::*;

pub struct PostgresDriver;

#[async_trait]
impl StoreDriver for PostgresDriver {
    fn kind(&self) -> &'static str {
        "postgres"
    }

    fn validate(&self, doc: &StoreDoc, mode: DocMode) -> Result<(), ValidationError> {
        match mode {
            DocMode::Seed => {
                let entries = doc.as_array().ok_or_else(|| {
                    ValidationError::new("seed.postgres", "must be a list of entries")
                })?;
                for (i, e) in entries.iter().enumerate() {
                    let path = format!("seed.postgres[{i}]");
                    let m = e
                        .as_object()
                        .ok_or_else(|| ValidationError::new(&path, "must be a mapping"))?;
                    let known = ["table", "rows", "conflict", "sql", "sql_file"];
                    for k in m.keys() {
                        if !known.contains(&k.as_str()) {
                            return Err(ValidationError::new(&path, format!("unknown key `{k}`")));
                        }
                    }
                    if m.contains_key("table") && !m.contains_key("rows") {
                        return Err(ValidationError::new(&path, "`table` needs `rows`"));
                    }
                    if !m.contains_key("table") && !m.contains_key("sql") && !m.contains_key("sql_file") {
                        return Err(ValidationError::new(&path, "needs table+rows, sql, or sql_file"));
                    }
                }
                Ok(())
            }
            DocMode::Verify => {
                let entries = doc.as_array().ok_or_else(|| {
                    ValidationError::new("verify.postgres", "must be a list of entries")
                })?;
                for (i, e) in entries.iter().enumerate() {
                    let path = format!("verify.postgres[{i}]");
                    let m = e
                        .as_object()
                        .ok_or_else(|| ValidationError::new(&path, "must be a mapping"))?;
                    if !m.contains_key("table") {
                        return Err(ValidationError::new(&path, "needs `table`"));
                    }
                    if !m.contains_key("expect") && !m.contains_key("expect_absent") {
                        return Err(ValidationError::new(&path, "needs `expect` or `expect_absent`"));
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn connect(&self, cfg: &StoreConnConfig) -> Result<Arc<dyn StateStore>, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&cfg.url)
            .await
            .map_err(|e| StoreError::Connection(format!("postgres: {e}")))?;
        Ok(Arc::new(PostgresStore {
            alias: cfg.alias.clone(),
            pool,
            suite_root: cfg
                .options
                .get("suite_root")
                .and_then(Value::as_str)
                .unwrap_or(".")
                .to_string(),
        }))
    }
}

pub struct PostgresStore {
    alias: String,
    pool: PgPool,
    suite_root: String,
}

#[async_trait]
impl StateStore for PostgresStore {
    fn kind(&self) -> &'static str {
        "postgres"
    }
    fn alias(&self) -> &str {
        &self.alias
    }

    async fn ping(&self) -> Result<(), StoreError> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|e| StoreError::Connection(format!("postgres: {e}")))
    }

    async fn reset(&self, spec: &StoreDoc) -> Result<(), StoreError> {
        let mode = spec.get("mode").and_then(Value::as_str).unwrap_or("truncate");
        if mode == "none" {
            return Ok(());
        }
        let exclude: Vec<String> = spec
            .get("exclude")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_else(|| vec!["schema_migrations".into(), "_sqlx_migrations".into()]);

        let rows = sqlx::query(
            "SELECT tablename FROM pg_tables WHERE schemaname = 'public'",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let tables: Vec<String> = rows
            .iter()
            .map(|r| r.get::<String, _>(0))
            .filter(|t| !exclude.contains(t))
            .collect();
        if tables.is_empty() {
            return Ok(());
        }
        let list = tables.iter().map(|t| quote_ident(t)).collect::<Vec<_>>().join(", ");
        sqlx::query(sqlx::AssertSqlSafe(format!("TRUNCATE {list} RESTART IDENTITY CASCADE")))
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn seed(&self, doc: &StoreDoc) -> Result<SeedReceipt, StoreError> {
        let entries = doc
            .as_array()
            .ok_or_else(|| StoreError::Harness("seed.postgres must be a list".into()))?;
        let mut receipt = SeedReceipt::default();
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        for entry in entries {
            if let Some(sql) = entry.get("sql").and_then(Value::as_str) {
                sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string())).execute(&mut *tx).await.map_err(db_err)?;
                receipt.entries.push("postgres: executed inline sql".into());
            } else if let Some(file) = entry.get("sql_file").and_then(Value::as_str) {
                let path = std::path::Path::new(&self.suite_root).join(file);
                let sql = std::fs::read_to_string(&path).map_err(|e| {
                    StoreError::Harness(format!("seed sql_file {}: {e}", path.display()))
                })?;
                sqlx::raw_sql(sqlx::AssertSqlSafe(sql)).execute(&mut *tx).await.map_err(db_err)?;
                receipt.entries.push(format!("postgres: executed {file}"));
            } else if let Some(table) = entry.get("table").and_then(Value::as_str) {
                let rows = entry
                    .get("rows")
                    .and_then(Value::as_array)
                    .ok_or_else(|| StoreError::Harness(format!("seed table `{table}`: rows missing")))?;
                let conflict = entry.get("conflict").and_then(Value::as_str).unwrap_or("error");
                for row in rows {
                    insert_row(&mut tx, table, row, conflict).await?;
                }
                receipt.entries.push(format!("postgres: {table} +{} rows", rows.len()));
            }
        }
        tx.commit().await.map_err(db_err)?;
        Ok(receipt)
    }

    async fn snapshot(&self, doc: &StoreDoc) -> Result<Snapshot, StoreError> {
        let tables = doc
            .as_array()
            .ok_or_else(|| StoreError::Harness("watch doc must be a table list".into()))?;
        let mut snap = Map::new();
        for t in tables {
            let Some(table) = t.as_str() else { continue };
            snap.insert(table.to_string(), json!(self.fetch_rows(table).await?));
        }
        Ok(Value::Object(snap))
    }

    async fn diff_snapshot(
        &self,
        doc: &StoreDoc,
        before: &Snapshot,
    ) -> Result<VerifyOutcome, StoreError> {
        let mut out = VerifyOutcome::default();
        let tables = doc.as_array().cloned().unwrap_or_default();
        for t in &tables {
            let Some(table) = t.as_str() else { continue };
            let after = self.fetch_rows(table).await?;
            let before_rows: Vec<Value> = before
                .get(table)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            let key = |v: &Value| v.to_string();
            let before_keys: std::collections::HashSet<String> =
                before_rows.iter().map(key).collect();
            let after_keys: std::collections::HashSet<String> = after.iter().map(key).collect();

            for row in &after {
                if !before_keys.contains(&key(row)) {
                    out.checks.push(CheckResult::Fail(CheckFailure::new(
                        format!("watch: table `{table}` gained a row"),
                        format!("watch.{table}"),
                        Value::Null,
                        FailureKind::UnexpectedChange { before: Value::Null, after: row.clone() },
                    )));
                }
            }
            for row in &before_rows {
                if !after_keys.contains(&key(row)) {
                    out.checks.push(CheckResult::Fail(CheckFailure::new(
                        format!("watch: table `{table}` lost a row"),
                        format!("watch.{table}"),
                        Value::Null,
                        FailureKind::UnexpectedChange { before: row.clone(), after: Value::Null },
                    )));
                }
            }
        }
        if out.checks.is_empty() {
            out.push_pass("watch: no unexplained changes");
        }
        Ok(out)
    }

    async fn verify(&self, doc: &StoreDoc, opts: &VerifyOpts) -> Result<VerifyOutcome, StoreError> {
        let ctx = MatchCtx { anchor_unix_ms: opts.anchor_unix_ms };
        let entries = doc
            .as_array()
            .ok_or_else(|| StoreError::Harness("verify.postgres must be a list".into()))?;
        let mut out = VerifyOutcome::default();

        for (ei, entry) in entries.iter().enumerate() {
            let Some(table) = entry.get("table").and_then(Value::as_str) else {
                continue;
            };
            let rows = self.fetch_rows(table).await?;
            let yaml_path = format!("verify.postgres[{ei}]");

            if let Some(expected) = entry.get("expect").and_then(Value::as_array) {
                verify_expected_rows(table, &yaml_path, expected, entry, &rows, &ctx, &mut out);
            }
            if let Some(absent) = entry.get("expect_absent").and_then(Value::as_array) {
                for (ai, exp) in absent.iter().enumerate() {
                    match rows.iter().find(|r| row_matches(exp, r, &ctx)) {
                        Some(found) => out.checks.push(CheckResult::Fail(CheckFailure::new(
                            format!("table `{table}`: row expected absent is present"),
                            format!("{yaml_path}.expect_absent[{ai}]"),
                            exp.clone(),
                            FailureKind::UnexpectedRow { actual: found.clone() },
                        ))),
                        None => out.push_pass(format!("{table}: absent row confirmed")),
                    }
                }
            }
        }
        Ok(out)
    }

    async fn inspect(&self, query: &str) -> Result<Table, StoreError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        sqlx::query("SET TRANSACTION READ ONLY").execute(&mut *tx).await.map_err(db_err)?;
        let wrapped = format!("SELECT to_jsonb(q) AS r FROM ({query}) q");
        let rows = sqlx::query(sqlx::AssertSqlSafe(wrapped)).fetch_all(&mut *tx).await.map_err(db_err)?;
        tx.rollback().await.ok();

        let mut table = Table::default();
        for row in rows {
            let v: Value = row.get("r");
            if let Some(obj) = v.as_object() {
                if table.columns.is_empty() {
                    table.columns = obj.keys().cloned().collect();
                }
                table.rows.push(table.columns.iter().map(|c| obj[c].clone()).collect());
            }
        }
        Ok(table)
    }
}

impl PostgresStore {
    async fn fetch_rows(&self, table: &str) -> Result<Vec<Value>, StoreError> {
        let sql = format!("SELECT to_jsonb(t) AS r FROM {} t LIMIT 1000", quote_ident(table));
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows.iter().map(|r| r.get::<Value, _>("r")).collect())
    }
}

/// Assignment of actual rows to expected rows runs as maximum bipartite
/// matching: greedy claiming can starve an expectation whose only candidate
/// was taken by an overlapping, laxer expectation.
fn verify_expected_rows(
    table: &str,
    yaml_path: &str,
    expected: &[Value],
    entry: &Value,
    rows: &[Value],
    ctx: &MatchCtx,
    out: &mut VerifyOutcome,
) {
    let adj: Vec<Vec<usize>> = expected
        .iter()
        .map(|exp| {
            rows.iter()
                .enumerate()
                .filter(|(_, r)| row_matches(exp, r, ctx))
                .map(|(j, _)| j)
                .collect()
        })
        .collect();
    let assignment = matchers::max_bipartite(rows.len(), &adj);

    let mut claimed = vec![false; rows.len()];
    for (i, exp) in expected.iter().enumerate() {
        match assignment[i] {
            Some(j) => {
                claimed[j] = true;
                out.push_pass(format!("{table}: row matched — {}", identity_summary(exp)));
            }
            None => {
                let mut failure = CheckFailure::new(
                    format!("table `{table}`: expected row MISSING — {}", identity_summary(exp)),
                    format!("{yaml_path}.expect[{i}]"),
                    exp.clone(),
                    FailureKind::MissingRow,
                );
                failure.near_misses = rank_near_misses(exp, rows, &claimed, ctx);
                out.checks.push(CheckResult::Fail(failure));
            }
        }
    }

    let count_mode = entry.get("count").and_then(Value::as_str).unwrap_or("at_least");
    if count_mode == "exact" {
        for (j, row) in rows.iter().enumerate() {
            if claimed[j] {
                continue;
            }
            let in_key_space = expected.iter().any(|exp| identity_contains(exp, row, ctx));
            if in_key_space {
                out.checks.push(CheckResult::Fail(CheckFailure::new(
                    format!("table `{table}`: unexpected extra row in expected key-space"),
                    format!("{yaml_path}.count"),
                    json!("count: exact"),
                    FailureKind::UnexpectedRow { actual: row.clone() },
                )));
            }
        }
    }
}

fn row_matches(expected: &Value, actual: &Value, ctx: &MatchCtx) -> bool {
    let Some(exp) = expected.as_object() else { return false };
    exp.iter().all(|(col, matcher)| {
        matchers::matches_value(matcher, actual.get(col), ctx)
    })
}

/// Identity columns are the literal (non-matcher) values — the part that says
/// WHICH row we mean, as opposed to what it should contain.
fn identity_of(expected: &Value) -> Map<String, Value> {
    expected
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, v)| !matchers::is_matcher_map(v) && !matchers::is_tag_matcher(v))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn identity_contains(expected: &Value, row: &Value, ctx: &MatchCtx) -> bool {
    identity_of(expected)
        .iter()
        .all(|(col, v)| matchers::matches_value(v, row.get(col), ctx))
}

fn identity_summary(expected: &Value) -> String {
    let id = identity_of(expected);
    if id.is_empty() {
        return "(matcher-only row)".into();
    }
    id.iter()
        .take(3)
        .map(|(k, v)| format!("{k}={}", compact(v)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn rank_near_misses(
    expected: &Value,
    rows: &[Value],
    claimed: &[bool],
    ctx: &MatchCtx,
) -> Vec<NearMiss> {
    let mut misses: Vec<NearMiss> = rows
        .iter()
        .enumerate()
        .filter(|(j, _)| !claimed[*j])
        .map(|(_, row)| {
            let (score, diffs) = matchers::score_object(expected, row, ctx);
            NearMiss { actual: row.clone(), diffs, score }
        })
        .filter(|nm| nm.score >= 0.5)
        .collect();
    misses.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    misses.truncate(3);
    misses
}

async fn insert_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    row: &Value,
    conflict: &str,
) -> Result<(), StoreError> {
    let cols: Vec<String> = row
        .as_object()
        .ok_or_else(|| StoreError::Harness(format!("seed `{table}`: row must be a mapping")))?
        .keys()
        .cloned()
        .collect();
    let col_list = cols.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
    // jsonb_populate_record derives column types server-side, so YAML values
    // land in timestamptz/uuid/numeric/jsonb columns without client-side casts.
    let mut sql = format!(
        "INSERT INTO {t} ({col_list}) SELECT {col_list} FROM jsonb_populate_record(NULL::{t}, $1)",
        t = quote_ident(table)
    );
    if conflict == "ignore" {
        sql.push_str(" ON CONFLICT DO NOTHING");
    }
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(row)
        .execute(&mut **tx)
        .await
        .map_err(|e| StoreError::Harness(format!("seed `{table}` row {row}: {e}")))?;
    Ok(())
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn compact(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn db_err(e: sqlx::Error) -> StoreError {
    StoreError::Harness(format!("postgres: {e}"))
}
