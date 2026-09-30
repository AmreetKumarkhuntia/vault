//! Postgres driver: seeding, TRUNCATE isolation, and expectation-driven
//! verification with near-miss reporting.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use globset::GlobBuilder;
use serde_json::{json, Map, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use vault_store::matchers::{self, MatchCtx};
use vault_store::*;
use walkdir::WalkDir;

const PREPARED_SQL_FILE_KEY: &str = "__vault_prepared_sql_file";

pub struct PostgresDriver;

#[async_trait]
impl StoreDriver for PostgresDriver {
    fn kind(&self) -> &'static str {
        "postgres"
    }

    fn validate(&self, doc: &StoreDoc, mode: DocMode) -> Result<(), ValidationError> {
        self.validate_impl(doc, mode)
    }

    fn validate_with_suite_root(
        &self,
        doc: &StoreDoc,
        mode: DocMode,
        suite_root: &Path,
    ) -> Result<(), ValidationError> {
        self.validate_with_context(doc, mode, &StoreDocContext::for_suite_root(suite_root))
    }

    fn validate_with_context(
        &self,
        doc: &StoreDoc,
        mode: DocMode,
        context: &StoreDocContext,
    ) -> Result<(), ValidationError> {
        self.prepare_impl(doc, mode, context).map(|_| ())
    }

    fn prepare_with_context(
        &self,
        doc: &StoreDoc,
        mode: DocMode,
        context: &StoreDocContext,
    ) -> Result<PreparedStoreDoc, ValidationError> {
        self.prepare_impl(doc, mode, context)
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
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")),
        }))
    }
}

impl PostgresDriver {
    fn validate_impl(&self, doc: &StoreDoc, mode: DocMode) -> Result<(), ValidationError> {
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
                    let known = ["table", "rows", "conflict", "sql", "sql_file", "sql_glob"];
                    for k in m.keys() {
                        if !known.contains(&k.as_str()) {
                            return Err(ValidationError::new(&path, format!("unknown key `{k}`")));
                        }
                    }

                    let variants: Vec<&str> = ["table", "sql", "sql_file", "sql_glob"]
                        .into_iter()
                        .filter(|key| m.contains_key(*key))
                        .collect();
                    if variants.len() != 1 {
                        return Err(ValidationError::new(
                            &path,
                            "needs exactly one of `table`, `sql`, `sql_file`, or `sql_glob`",
                        ));
                    }

                    match variants[0] {
                        "table" => {
                            non_empty_string(m.get("table"), &format!("{path}.table"))?;
                            let rows =
                                m.get("rows").and_then(Value::as_array).ok_or_else(|| {
                                    ValidationError::new(format!("{path}.rows"), "must be a list")
                                })?;
                            for (row_index, row) in rows.iter().enumerate() {
                                if !row.is_object() {
                                    return Err(ValidationError::new(
                                        format!("{path}.rows[{row_index}]"),
                                        "must be a mapping",
                                    ));
                                }
                            }
                            if m.contains_key("conflict") {
                                non_empty_string(m.get("conflict"), &format!("{path}.conflict"))?;
                            }
                        }
                        "sql" => {
                            reject_table_options(m, &path)?;
                            let sql = non_empty_string(m.get("sql"), &format!("{path}.sql"))?;
                            reject_transaction_control(sql).map_err(|command| {
                                ValidationError::new(
                                    format!("{path}.sql"),
                                    transaction_control_message(command),
                                )
                            })?;
                        }
                        "sql_file" => {
                            reject_table_options(m, &path)?;
                            let logical =
                                non_empty_string(m.get("sql_file"), &format!("{path}.sql_file"))?;
                            validate_sql_file_reference(logical).map_err(|message| {
                                ValidationError::new(
                                    format!("{path}.sql_file"),
                                    format!("`{logical}`: {message}"),
                                )
                            })?;
                        }
                        "sql_glob" => {
                            reject_table_options(m, &path)?;
                            let selector =
                                non_empty_string(m.get("sql_glob"), &format!("{path}.sql_glob"))?;
                            validate_sql_glob_reference(selector).map_err(|message| {
                                ValidationError::new(
                                    format!("{path}.sql_glob"),
                                    format!("`{selector}`: {message}"),
                                )
                            })?;
                        }
                        _ => unreachable!("validated postgres seed variant"),
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
                        return Err(ValidationError::new(
                            &path,
                            "needs `expect` or `expect_absent`",
                        ));
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn prepare_impl(
        &self,
        doc: &StoreDoc,
        mode: DocMode,
        context: &StoreDocContext,
    ) -> Result<PreparedStoreDoc, ValidationError> {
        self.validate_impl(doc, mode)?;
        if mode != DocMode::Seed {
            return Ok(PreparedStoreDoc::passthrough(doc.clone()));
        }

        let entries = doc
            .as_array()
            .expect("seed list was checked by validate_impl");
        let base_dir = entries
            .iter()
            .filter_map(|entry| {
                entry
                    .get("sql_file")
                    .or_else(|| entry.get("sql_glob"))
                    .and_then(Value::as_str)
            })
            .any(selector_is_relative)
            .then(|| absolute_declaration_dir(context))
            .transpose()?;
        let mut prepared_entries = Vec::new();
        let mut files = Vec::new();
        let mut seen: HashMap<PathBuf, ResolvedFileSource> = HashMap::new();

        for (source_index, entry) in entries.iter().enumerate() {
            let map = entry
                .as_object()
                .expect("seed mapping was checked by validate_impl");
            if let Some(selector) = map.get("sql_file").and_then(Value::as_str) {
                let yaml_path = format!("seed.postgres[{source_index}].sql_file");
                let selection = resolve_exact_sql_file(
                    selector,
                    source_index,
                    &yaml_path,
                    context,
                    base_dir
                        .as_deref()
                        .unwrap_or_else(|| context.declaring_dir()),
                )?;
                push_prepared_file(selection, &mut prepared_entries, &mut files, &mut seen)?;
            } else if let Some(selector) = map.get("sql_glob").and_then(Value::as_str) {
                let yaml_path = format!("seed.postgres[{source_index}].sql_glob");
                for selection in expand_sql_glob(
                    selector,
                    source_index,
                    &yaml_path,
                    context,
                    base_dir
                        .as_deref()
                        .unwrap_or_else(|| context.declaring_dir()),
                )? {
                    push_prepared_file(selection, &mut prepared_entries, &mut files, &mut seen)?;
                }
            } else {
                prepared_entries.push(entry.clone());
            }
        }

        Ok(PreparedStoreDoc::new(Value::Array(prepared_entries), files))
    }
}

pub struct PostgresStore {
    alias: String,
    pool: PgPool,
    suite_root: PathBuf,
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
        let mode = spec
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("truncate");
        if mode == "none" {
            return Ok(());
        }
        let exclude: Vec<String> = spec
            .get("exclude")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec!["schema_migrations".into(), "_sqlx_migrations".into()]);

        let rows = sqlx::query("SELECT tablename FROM pg_tables WHERE schemaname = 'public'")
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
        let list = tables
            .iter()
            .map(|t| quote_ident(t))
            .collect::<Vec<_>>()
            .join(", ");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "TRUNCATE {list} RESTART IDENTITY CASCADE"
        )))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn seed(&self, doc: &StoreDoc) -> Result<SeedReceipt, StoreError> {
        let prepared = if doc.as_array().is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry.get(PREPARED_SQL_FILE_KEY).is_some())
        }) {
            None
        } else {
            Some(
                PostgresDriver
                    .prepare_with_context(
                        doc,
                        DocMode::Seed,
                        &StoreDocContext::for_suite_root(&self.suite_root),
                    )
                    .map_err(|error| StoreError::Harness(error.to_string()))?
                    .into_doc(),
            )
        };
        let execution_doc = prepared.as_ref().unwrap_or(doc);
        let entries = execution_doc
            .as_array()
            .ok_or_else(|| StoreError::Harness("seed.postgres must be a list".into()))?;
        let mut receipt = SeedReceipt::default();
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        let execution: Result<(), StoreError> = async {
            validate_prepared_fixture_identities(entries)?;
            for (index, entry) in entries.iter().enumerate() {
                if entry.get(PREPARED_SQL_FILE_KEY).is_some() {
                    let source = prepared_file_source(entry, index)?;
                    let (path, sql) = runtime_sql_file(&source)?;
                    set_standard_conforming_strings(&mut tx)
                        .await
                        .map_err(|error| {
                            StoreError::Harness(format!(
                                "{} {} fixture `{}` resolved to `{}`: could not enforce standard string parsing: {error}",
                                source.yaml_path,
                                source.scope.as_str(),
                                source.logical_path.display(),
                                path.display()
                            ))
                        })?;
                    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                        .execute(&mut *tx)
                        .await
                        .map_err(|error| {
                            StoreError::Harness(format!(
                                "{} {} fixture `{}` selected by {} `{}` in `{}` resolved to `{}`: postgres: {error}",
                                source.yaml_path,
                                source.scope.as_str(),
                                source.logical_path.display(),
                                source.selector_kind,
                                source.selector,
                                source.declaring_yaml.display(),
                                path.display()
                            ))
                        })?;
                    receipt.entries.push(format!(
                        "postgres: executed {}",
                        source.logical_path.display()
                    ));
                } else if let Some(sql) = entry.get("sql").and_then(Value::as_str) {
                    reject_transaction_control(sql).map_err(|command| {
                        StoreError::Harness(format!(
                            "seed.postgres[{index}].sql: {}",
                            transaction_control_message(command)
                        ))
                    })?;
                    set_standard_conforming_strings(&mut tx)
                        .await
                        .map_err(|e| {
                            StoreError::Harness(format!(
                                "seed.postgres[{index}].sql: could not enforce standard string parsing: {e}"
                            ))
                        })?;
                    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| {
                            StoreError::Harness(format!(
                                "seed.postgres[{index}].sql: postgres: {e}"
                            ))
                        })?;
                    receipt.entries.push("postgres: executed inline sql".into());
                } else if let Some(table) = entry.get("table").and_then(Value::as_str) {
                    let rows = entry.get("rows").and_then(Value::as_array).ok_or_else(|| {
                        StoreError::Harness(format!(
                            "seed.postgres[{index}] table `{table}`: rows missing"
                        ))
                    })?;
                    let conflict = entry
                        .get("conflict")
                        .and_then(Value::as_str)
                        .unwrap_or("error");
                    for row in rows {
                        insert_row(&mut tx, table, row, conflict).await?;
                    }
                    receipt
                        .entries
                        .push(format!("postgres: {table} +{} rows", rows.len()));
                } else {
                    return Err(StoreError::Harness(format!(
                        "seed.postgres[{index}]: invalid prepared seed entry"
                    )));
                }
            }
            Ok(())
        }
        .await;

        if let Err(error) = execution {
            return match tx.rollback().await {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(StoreError::Harness(format!(
                    "{error}; rollback failed: postgres: {rollback_error}"
                ))),
            };
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
                        FailureKind::UnexpectedChange {
                            before: Value::Null,
                            after: row.clone(),
                        },
                    )));
                }
            }
            for row in &before_rows {
                if !after_keys.contains(&key(row)) {
                    out.checks.push(CheckResult::Fail(CheckFailure::new(
                        format!("watch: table `{table}` lost a row"),
                        format!("watch.{table}"),
                        Value::Null,
                        FailureKind::UnexpectedChange {
                            before: row.clone(),
                            after: Value::Null,
                        },
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
        let ctx = MatchCtx {
            anchor_unix_ms: opts.anchor_unix_ms,
        };
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
                            FailureKind::UnexpectedRow {
                                actual: found.clone(),
                            },
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
        sqlx::query("SET TRANSACTION READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        let wrapped = format!("SELECT to_jsonb(q) AS r FROM ({query}) q");
        let rows = sqlx::query(sqlx::AssertSqlSafe(wrapped))
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.rollback().await.ok();

        let mut table = Table::default();
        for row in rows {
            let v: Value = row.get("r");
            if let Some(obj) = v.as_object() {
                if table.columns.is_empty() {
                    table.columns = obj.keys().cloned().collect();
                }
                table
                    .rows
                    .push(table.columns.iter().map(|c| obj[c].clone()).collect());
            }
        }
        Ok(table)
    }
}

fn non_empty_string<'a>(
    value: Option<&'a Value>,
    yaml_path: &str,
) -> Result<&'a str, ValidationError> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| ValidationError::new(yaml_path, "must be a string"))?;
    if value.trim().is_empty() {
        return Err(ValidationError::new(yaml_path, "must not be empty"));
    }
    Ok(value)
}

fn reject_table_options(
    entry: &Map<String, Value>,
    yaml_path: &str,
) -> Result<(), ValidationError> {
    for key in ["rows", "conflict"] {
        if entry.contains_key(key) {
            return Err(ValidationError::new(
                format!("{yaml_path}.{key}"),
                "is only valid with `table`",
            ));
        }
    }
    Ok(())
}

fn read_sql_file(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

async fn set_standard_conforming_strings(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), sqlx::Error> {
    // Plain-string backslash semantics are a server setting. Pin them before
    // every raw entry so the offline lexer and PostgreSQL parse the same text,
    // even if a preceding fixture changed the session setting.
    sqlx::query("SET LOCAL standard_conforming_strings = on")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn transaction_control_message(command: &str) -> String {
    format!(
        "top-level transaction-control statement `{command}` is not allowed; Vault manages the seed transaction"
    )
}

/// Reject commands that can create, end, or otherwise take ownership of the
/// transaction wrapped around a Postgres seed document. This is deliberately
/// a lexer rather than a substring search: transaction keywords in data,
/// comments, identifiers, and function bodies are not statements.
fn reject_transaction_control(sql: &str) -> Result<(), &'static str> {
    const TOKEN_LIMIT: usize = 5;

    let bytes = sql.as_bytes();
    let mut tokens = Vec::with_capacity(TOKEN_LIMIT);
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"--") {
            index = skip_line_comment(bytes, index + 2);
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            index = skip_block_comment(bytes, index + 2);
            continue;
        }
        if bytes[index] == b';' {
            if let Some(command) = transaction_control_command(&tokens) {
                return Err(command);
            }
            tokens.clear();
            index += 1;
            continue;
        }
        if bytes[index] == b'\'' {
            push_sql_token(&mut tokens, "<literal>", TOKEN_LIMIT);
            let escape_backslashes = is_escape_string_prefix(bytes, index);
            index = skip_single_quoted(bytes, index + 1, escape_backslashes);
            continue;
        }
        if bytes[index] == b'"' {
            push_sql_token(&mut tokens, "<identifier>", TOKEN_LIMIT);
            index = skip_double_quoted(bytes, index + 1);
            continue;
        }
        if bytes[index] == b'$' {
            if let Some(delimiter_end) = dollar_quote_delimiter_end(bytes, index) {
                push_sql_token(&mut tokens, "<dollar-quoted>", TOKEN_LIMIT);
                index = skip_dollar_quoted(bytes, index, delimiter_end);
                continue;
            }
        }
        if is_identifier_start(bytes[index]) {
            let start = index;
            index += 1;
            while index < bytes.len() && is_identifier_continue(bytes[index]) {
                index += 1;
            }
            if tokens.len() < TOKEN_LIMIT {
                tokens.push(String::from_utf8_lossy(&bytes[start..index]).to_ascii_uppercase());
            }
            continue;
        }

        push_sql_token(&mut tokens, "<other>", TOKEN_LIMIT);
        index += 1;
    }

    match transaction_control_command(&tokens) {
        Some(command) => Err(command),
        None => Ok(()),
    }
}

fn push_sql_token(tokens: &mut Vec<String>, token: &str, limit: usize) {
    if tokens.len() < limit {
        tokens.push(token.to_string());
    }
}

fn transaction_control_command(tokens: &[String]) -> Option<&'static str> {
    let first = tokens.first()?.as_str();
    match first {
        "BEGIN" => Some("BEGIN"),
        "START" if token_is(tokens, 1, "TRANSACTION") => Some("START TRANSACTION"),
        "COMMIT" => Some("COMMIT"),
        "END" => Some("END"),
        "ROLLBACK" => Some("ROLLBACK"),
        "ABORT" => Some("ABORT"),
        "SAVEPOINT" => Some("SAVEPOINT"),
        "RELEASE" => Some("RELEASE SAVEPOINT"),
        "PREPARE" if token_is(tokens, 1, "TRANSACTION") => Some("PREPARE TRANSACTION"),
        "SET" if token_is(tokens, 1, "TRANSACTION") => Some("SET TRANSACTION"),
        "SET" if token_is(tokens, 1, "LOCAL") && token_is(tokens, 2, "TRANSACTION") => {
            Some("SET LOCAL TRANSACTION")
        }
        "SET" if token_is(tokens, 1, "SESSION") && token_is(tokens, 2, "TRANSACTION") => {
            Some("SET SESSION TRANSACTION")
        }
        "SET"
            if token_is(tokens, 1, "SESSION")
                && token_is(tokens, 2, "CHARACTERISTICS")
                && token_is(tokens, 3, "AS")
                && token_is(tokens, 4, "TRANSACTION") =>
        {
            Some("SET SESSION CHARACTERISTICS AS TRANSACTION")
        }
        "SET" if transaction_setting_after_optional_scope(tokens, 1).is_some() => {
            Some("SET transaction configuration")
        }
        "RESET" if token_is(tokens, 1, "ALL") || transaction_setting_token(tokens, 1) => {
            Some("RESET transaction configuration")
        }
        _ => None,
    }
}

fn transaction_setting_after_optional_scope(tokens: &[String], index: usize) -> Option<&str> {
    let index = if token_is(tokens, index, "LOCAL") || token_is(tokens, index, "SESSION") {
        index + 1
    } else {
        index
    };
    transaction_setting_token(tokens, index).then(|| tokens[index].as_str())
}

fn transaction_setting_token(tokens: &[String], index: usize) -> bool {
    matches!(
        tokens.get(index).map(String::as_str),
        Some(
            "DEFAULT_TRANSACTION_READ_ONLY"
                | "DEFAULT_TRANSACTION_ISOLATION"
                | "DEFAULT_TRANSACTION_DEFERRABLE"
                | "TRANSACTION_READ_ONLY"
                | "TRANSACTION_ISOLATION"
                | "TRANSACTION_DEFERRABLE"
        )
    )
}

fn token_is(tokens: &[String], index: usize, expected: &str) -> bool {
    tokens.get(index).is_some_and(|token| token == expected)
}

fn skip_line_comment(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
        index += 1;
    }
    index
}

fn skip_block_comment(bytes: &[u8], mut index: usize) -> usize {
    let mut depth = 1usize;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"/*") {
            depth += 1;
            index += 2;
        } else if bytes[index..].starts_with(b"*/") {
            depth -= 1;
            index += 2;
            if depth == 0 {
                break;
            }
        } else {
            index += 1;
        }
    }
    index
}

fn skip_single_quoted(bytes: &[u8], mut index: usize, escape_backslashes: bool) -> usize {
    while index < bytes.len() {
        if escape_backslashes && bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
        } else if bytes[index] == b'\'' {
            if bytes.get(index + 1) == Some(&b'\'') {
                index += 2;
            } else {
                return index + 1;
            }
        } else {
            index += 1;
        }
    }
    index
}

fn skip_double_quoted(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() {
        if bytes[index] == b'"' {
            if bytes.get(index + 1) == Some(&b'"') {
                index += 2;
            } else {
                return index + 1;
            }
        } else {
            index += 1;
        }
    }
    index
}

fn is_escape_string_prefix(bytes: &[u8], quote_index: usize) -> bool {
    quote_index > 0
        && matches!(bytes[quote_index - 1], b'e' | b'E')
        && (quote_index == 1 || !is_identifier_continue(bytes[quote_index - 2]))
}

fn dollar_quote_delimiter_end(bytes: &[u8], start: usize) -> Option<usize> {
    if start > 0 && is_identifier_continue(bytes[start - 1]) {
        return None;
    }
    let mut index = start + 1;
    if bytes.get(index) == Some(&b'$') {
        return Some(index + 1);
    }
    if !bytes
        .get(index)
        .is_some_and(|byte| is_identifier_start(*byte))
    {
        return None;
    }
    index += 1;
    while bytes
        .get(index)
        .is_some_and(|byte| is_dollar_tag_continue(*byte))
    {
        index += 1;
    }
    (bytes.get(index) == Some(&b'$')).then_some(index + 1)
}

fn skip_dollar_quoted(bytes: &[u8], start: usize, delimiter_end: usize) -> usize {
    let delimiter = &bytes[start..delimiter_end];
    let mut index = delimiter_end;
    while index + delimiter.len() <= bytes.len() {
        if &bytes[index..index + delimiter.len()] == delimiter {
            return index + delimiter.len();
        }
        index += 1;
    }
    bytes.len()
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || !byte.is_ascii()
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit() || byte == b'$'
}

fn is_dollar_tag_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn validate_sql_file_reference(logical_path: &str) -> Result<(), String> {
    if logical_path.contains("{{") || logical_path.contains("}}") {
        return Err("must be a static path without template expressions".into());
    }
    if Path::new(logical_path)
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("sql")
    {
        return Err("must have a `.sql` extension".into());
    }
    Ok(())
}

fn validate_sql_glob_reference(selector: &str) -> Result<(), String> {
    if selector.contains("{{") || selector.contains("}}") {
        return Err("must be a static path without template expressions".into());
    }
    if selector.contains('\\') {
        return Err("must use `/` path separators".into());
    }
    if selector
        .chars()
        .any(|character| matches!(character, '[' | ']' | '{' | '}'))
    {
        return Err("supports only `*`, `?`, and `**` wildcard syntax".into());
    }
    if selector.contains("***") {
        return Err("contains an invalid `***` wildcard; use `*` or `**`".into());
    }
    if selector
        .split('/')
        .any(|component| component.contains("**") && component != "**")
    {
        return Err("requires `**` to be a complete path segment for recursive matching".into());
    }
    let mut wildcard_seen = false;
    for component in selector.split('/') {
        if component == ".." && wildcard_seen {
            return Err(
                "requires parent (`..`) components to appear before the first wildcard".into(),
            );
        }
        wildcard_seen |= component.contains('*') || component.contains('?');
    }
    if !selector.contains('*') && !selector.contains('?') {
        return Err("must contain `*`, `?`, or `**`".into());
    }
    Ok(())
}

fn absolute_declaration_dir(context: &StoreDocContext) -> Result<PathBuf, ValidationError> {
    let directory = context.declaring_dir();
    let absolute = if directory.is_absolute() {
        directory.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                ValidationError::new(
                    "seed.postgres",
                    format!("could not resolve the process directory: {error}"),
                )
            })?
            .join(directory)
    };
    std::fs::metadata(&absolute).map_err(|error| {
        ValidationError::new(
            "seed.postgres",
            format!(
                "declaring YAML `{}` has an unresolved parent directory `{}`: {error}",
                context.declaring_yaml.display(),
                absolute.display()
            ),
        )
    })?;
    if !absolute.is_dir() {
        return Err(ValidationError::new(
            "seed.postgres",
            format!(
                "declaring YAML `{}` has a parent path `{}` that is not a directory",
                context.declaring_yaml.display(),
                absolute.display()
            ),
        ));
    }
    Ok(absolute)
}

fn resolve_exact_sql_file(
    selector: &str,
    source_index: usize,
    yaml_path: &str,
    context: &StoreDocContext,
    base_dir: &Path,
) -> Result<ResolvedFileSource, ValidationError> {
    validate_sql_file_reference(selector)
        .map_err(|message| ValidationError::new(yaml_path, format!("`{selector}`: {message}")))?;
    reject_foreign_absolute_path(selector, yaml_path)?;

    let selector_path = Path::new(selector);
    let candidate = match classify_selector_path(selector) {
        SelectorPathKind::NativeAbsolute => selector_path.to_path_buf(),
        SelectorPathKind::Relative => base_dir.join(selector_path),
        SelectorPathKind::ForeignAbsolute(_) | SelectorPathKind::Invalid(_) => {
            unreachable!("foreign and invalid paths were rejected above")
        }
    };
    let resolved = std::fs::canonicalize(&candidate).map_err(|error| {
        let migration_hint = legacy_suite_root_hint(selector, context);
        ValidationError::new(
            yaml_path,
            format!(
                "`{selector}` declared in `{}` resolved as `{}`: could not resolve SQL file: {error}{migration_hint}",
                context.declaring_yaml.display(),
                candidate.display()
            ),
        )
    })?;
    validate_resolved_sql_file(
        "sql_file",
        selector,
        selector,
        &candidate,
        &resolved,
        yaml_path,
        &context.declaring_yaml,
    )?;

    Ok(ResolvedFileSource {
        prepared_index: 0,
        source_index,
        yaml_path: yaml_path.to_string(),
        declaring_yaml: context.declaring_yaml.clone(),
        scope: context.scope,
        selector_kind: "sql_file".into(),
        selector: selector.into(),
        logical_path: selector_path.to_path_buf(),
        candidate_path: candidate,
        resolved_path: resolved,
    })
}

fn expand_sql_glob(
    selector: &str,
    source_index: usize,
    yaml_path: &str,
    context: &StoreDocContext,
    base_dir: &Path,
) -> Result<Vec<ResolvedFileSource>, ValidationError> {
    validate_sql_glob_reference(selector)
        .map_err(|message| ValidationError::new(yaml_path, format!("`{selector}`: {message}")))?;
    reject_foreign_absolute_path(selector, yaml_path)?;

    let selector_path = Path::new(selector);
    let (logical_fixed_prefix, wildcard_suffix) =
        split_glob_prefix(selector_path).ok_or_else(|| {
            ValidationError::new(yaml_path, format!("`{selector}` must contain a wildcard"))
        })?;
    let candidate_prefix = match classify_selector_path(selector) {
        SelectorPathKind::NativeAbsolute => logical_fixed_prefix.clone(),
        SelectorPathKind::Relative => base_dir.join(&logical_fixed_prefix),
        SelectorPathKind::ForeignAbsolute(_) | SelectorPathKind::Invalid(_) => {
            unreachable!("foreign and invalid paths were rejected above")
        }
    };
    let canonical_prefix = std::fs::canonicalize(&candidate_prefix).map_err(|error| {
        ValidationError::new(
            yaml_path,
            format!(
                "`{selector}` declared in `{}` has unresolved fixed prefix `{}`: {error}",
                context.declaring_yaml.display(),
                candidate_prefix.display()
            ),
        )
    })?;
    if !canonical_prefix.is_dir() {
        return Err(ValidationError::new(
            yaml_path,
            format!(
                "`{selector}` fixed prefix `{}` must resolve to a directory",
                canonical_prefix.display()
            ),
        ));
    }

    let wildcard_suffix_utf8 = utf8_path(&wildcard_suffix, yaml_path, "glob pattern")?;
    let matcher = GlobBuilder::new(&path_with_forward_slashes(wildcard_suffix_utf8))
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map_err(|error| {
            ValidationError::new(yaml_path, format!("invalid SQL glob `{selector}`: {error}"))
        })?
        .compile_matcher();

    let logical_prefix = logical_glob_prefix(selector_path);
    let mut matches = Vec::new();
    let recursive = wildcard_suffix
        .components()
        .any(|component| component.as_os_str() == "**");
    let mut walker = WalkDir::new(&canonical_prefix)
        .follow_links(false)
        .min_depth(1);
    if !recursive {
        walker = walker.max_depth(wildcard_suffix.components().count());
    }
    for walked in walker {
        let walked = walked.map_err(|error| {
            ValidationError::new(
                yaml_path,
                format!(
                    "could not traverse SQL glob `{selector}` from `{}`: {error}",
                    canonical_prefix.display()
                ),
            )
        })?;
        let relative_match = walked
            .path()
            .strip_prefix(&canonical_prefix)
            .map_err(|error| {
                ValidationError::new(
                    yaml_path,
                    format!(
                        "could not normalize match `{}`: {error}",
                        walked.path().display()
                    ),
                )
            })?;
        if !matcher.is_match(relative_match) {
            continue;
        }
        let logical_path = logical_prefix.join(relative_match);
        let candidate = candidate_prefix.join(relative_match);
        matches.push((logical_path, candidate));
    }

    matches.sort_by_key(|entry| path_sort_key(&entry.0));
    if matches.is_empty() {
        return Err(ValidationError::new(
            yaml_path,
            format!(
                "SQL glob `{selector}` declared in `{}` matched no files",
                context.declaring_yaml.display()
            ),
        ));
    }

    let mut selections = Vec::with_capacity(matches.len());
    for (logical_path, candidate) in matches {
        utf8_path(&logical_path, yaml_path, "matched logical path")?;
        utf8_path(&candidate, yaml_path, "matched path")?;
        let resolved = std::fs::canonicalize(&candidate).map_err(|error| {
            ValidationError::new(
                yaml_path,
                format!(
                    "`{selector}` matched `{}` but it could not be resolved: {error}",
                    logical_path.display()
                ),
            )
        })?;
        validate_resolved_sql_file(
            "sql_glob",
            selector,
            &logical_path.display().to_string(),
            &candidate,
            &resolved,
            yaml_path,
            &context.declaring_yaml,
        )?;
        selections.push(ResolvedFileSource {
            prepared_index: 0,
            source_index,
            yaml_path: yaml_path.to_string(),
            declaring_yaml: context.declaring_yaml.clone(),
            scope: context.scope,
            selector_kind: "sql_glob".into(),
            selector: selector.into(),
            logical_path,
            candidate_path: candidate,
            resolved_path: resolved,
        });
    }
    Ok(selections)
}

fn split_glob_prefix(pattern: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut fixed = PathBuf::new();
    let mut suffix = PathBuf::new();
    let mut wildcard_seen = false;
    for component in pattern.components() {
        let has_wildcard = matches!(component, Component::Normal(value) if {
            let text = value.to_string_lossy();
            text.contains('*') || text.contains('?')
        });
        if wildcard_seen || has_wildcard {
            wildcard_seen = true;
            suffix.push(component.as_os_str());
        } else {
            fixed.push(component.as_os_str());
        }
    }
    wildcard_seen.then_some((fixed, suffix))
}

fn logical_glob_prefix(selector: &Path) -> PathBuf {
    let mut prefix = PathBuf::new();
    for component in selector.components() {
        let has_wildcard = matches!(component, Component::Normal(value) if {
            let text = value.to_string_lossy();
            text.contains('*') || text.contains('?')
        });
        if has_wildcard {
            break;
        }
        prefix.push(component.as_os_str());
    }
    prefix
}

fn validate_resolved_sql_file(
    selector_kind: &str,
    selector: &str,
    logical: &str,
    candidate: &Path,
    resolved: &Path,
    yaml_path: &str,
    declaring_yaml: &Path,
) -> Result<(), ValidationError> {
    let declaring_yaml_text = utf8_path(declaring_yaml, yaml_path, "declaring YAML path")?;
    let candidate_text = utf8_path(candidate, yaml_path, "matched path")?;
    let resolved_text = utf8_path(resolved, yaml_path, "resolved path")?;
    for (description, value) in [
        ("matched logical path", logical),
        ("declaring YAML path", declaring_yaml_text),
        ("matched path", candidate_text),
        ("resolved path", resolved_text),
    ] {
        if value.contains("{{") || value.contains("}}") {
            return Err(ValidationError::new(
                yaml_path,
                format!(
                    "{selector_kind} `{selector}` declared in `{}` selected `{logical}`, but its {description} `{value}` contains a runtime template marker",
                    declaring_yaml.display()
                ),
            ));
        }
    }
    if candidate
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("sql")
    {
        return Err(ValidationError::new(
            yaml_path,
            format!(
                "{selector_kind} `{selector}` declared in `{}` selected `{logical}` resolved as `{}` which must have a lowercase `.sql` extension",
                declaring_yaml.display(),
                candidate.display()
            ),
        ));
    }
    if !resolved.is_file() {
        return Err(ValidationError::new(
            yaml_path,
            format!(
                "{selector_kind} `{selector}` declared in `{}` selected `{logical}` resolved to `{}` which must be a regular file",
                declaring_yaml.display(),
                resolved.display()
            ),
        ));
    }
    let sql = read_sql_file(resolved).map_err(|error| {
        ValidationError::new(
            yaml_path,
            format!(
                "{selector_kind} `{selector}` declared in `{}` selected `{logical}` resolved to `{}`: could not read UTF-8 SQL file: {error}",
                declaring_yaml.display(),
                resolved.display()
            ),
        )
    })?;
    reject_transaction_control(&sql).map_err(|command| {
        ValidationError::new(
            yaml_path,
            format!(
                "{selector_kind} `{selector}` declared in `{}` selected `{logical}` resolved to `{}`: {}",
                declaring_yaml.display(),
                resolved.display(),
                transaction_control_message(command)
            ),
        )
    })
}

fn push_prepared_file(
    mut source: ResolvedFileSource,
    entries: &mut Vec<Value>,
    files: &mut Vec<ResolvedFileSource>,
    seen: &mut HashMap<PathBuf, ResolvedFileSource>,
) -> Result<(), ValidationError> {
    if let Some(previous) = seen.get(&source.resolved_path) {
        return Err(ValidationError::new(
            &source.yaml_path,
            format!(
                "SQL fixture `{}` from {} `{}` declared in `{}` resolves to `{}`, already selected by {} `{}` at {} declared in `{}`",
                source.logical_path.display(),
                source.selector_kind,
                source.selector,
                source.declaring_yaml.display(),
                source.resolved_path.display(),
                previous.selector_kind,
                previous.selector,
                previous.yaml_path,
                previous.declaring_yaml.display()
            ),
        ));
    }
    source.prepared_index = entries.len();
    entries.push(prepared_file_entry(&source));
    seen.insert(source.resolved_path.clone(), source.clone());
    files.push(source);
    Ok(())
}

fn prepared_file_entry(source: &ResolvedFileSource) -> Value {
    json!({
        PREPARED_SQL_FILE_KEY: {
            "source_index": source.source_index,
            "yaml_path": source.yaml_path,
            "declaring_yaml": source.declaring_yaml.to_string_lossy(),
            "scope": source.scope.as_str(),
            "selector_kind": source.selector_kind,
            "selector": source.selector,
            "logical_path": source.logical_path.to_string_lossy(),
            "candidate_path": source.candidate_path.to_string_lossy(),
            "resolved_path": source.resolved_path.to_string_lossy(),
        }
    })
}

fn prepared_file_source(
    entry: &Value,
    prepared_index: usize,
) -> Result<ResolvedFileSource, StoreError> {
    let metadata = entry
        .get(PREPARED_SQL_FILE_KEY)
        .and_then(Value::as_object)
        .ok_or_else(|| {
            StoreError::Harness(format!(
                "seed.postgres[{prepared_index}]: invalid prepared SQL fixture metadata"
            ))
        })?;
    let string = |key: &str| -> Result<String, StoreError> {
        metadata
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                StoreError::Harness(format!(
                    "seed.postgres[{prepared_index}].{PREPARED_SQL_FILE_KEY}.{key}: missing string"
                ))
            })
    };
    let scope = match string("scope")?.as_str() {
        "global" => StoreDocScope::Global,
        "local" => StoreDocScope::Local,
        value => {
            return Err(StoreError::Harness(format!(
            "seed.postgres[{prepared_index}].{PREPARED_SQL_FILE_KEY}.scope: invalid scope `{value}`"
        )))
        }
    };
    Ok(ResolvedFileSource {
        prepared_index,
        source_index: metadata
            .get("source_index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| StoreError::Harness(format!(
                "seed.postgres[{prepared_index}].{PREPARED_SQL_FILE_KEY}.source_index: missing integer"
            )))?,
        yaml_path: string("yaml_path")?,
        declaring_yaml: PathBuf::from(string("declaring_yaml")?),
        scope,
        selector_kind: string("selector_kind")?,
        selector: string("selector")?,
        logical_path: PathBuf::from(string("logical_path")?),
        candidate_path: PathBuf::from(string("candidate_path")?),
        resolved_path: PathBuf::from(string("resolved_path")?),
    })
}

fn validate_prepared_fixture_identities(entries: &[Value]) -> Result<(), StoreError> {
    let has_prepared_files = entries
        .iter()
        .any(|entry| entry.get(PREPARED_SQL_FILE_KEY).is_some());
    if !has_prepared_files {
        return Ok(());
    }

    let mut seen: HashMap<PathBuf, ResolvedFileSource> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.get("sql_file").is_some() || entry.get("sql_glob").is_some() {
            return Err(StoreError::Harness(format!(
                "seed.postgres[{index}]: prepared fixture plan contains an unresolved sql_file or sql_glob entry"
            )));
        }
        if entry.get(PREPARED_SQL_FILE_KEY).is_none() {
            continue;
        }

        let source = prepared_file_source(entry, index)?;
        if let Some(previous) = seen.get(&source.resolved_path) {
            return Err(StoreError::Harness(format!(
                "{} {} fixture `{}` selected by {} `{}` in `{}` resolves to `{}`, already selected by {} `{}` at {} {} fixture `{}` in `{}`",
                source.yaml_path,
                source.scope.as_str(),
                source.logical_path.display(),
                source.selector_kind,
                source.selector,
                source.declaring_yaml.display(),
                source.resolved_path.display(),
                previous.selector_kind,
                previous.selector,
                previous.yaml_path,
                previous.scope.as_str(),
                previous.logical_path.display(),
                previous.declaring_yaml.display()
            )));
        }
        seen.insert(source.resolved_path.clone(), source);
    }
    Ok(())
}

fn runtime_sql_file(source: &ResolvedFileSource) -> Result<(PathBuf, String), StoreError> {
    let resolved = std::fs::canonicalize(&source.candidate_path).map_err(|error| {
        StoreError::Harness(format!(
            "{} {} fixture `{}` selected by {} `{}` in `{}` resolved as `{}` (prepared as `{}`): could not re-resolve SQL file: {error}",
            source.yaml_path,
            source.scope.as_str(),
            source.logical_path.display(),
            source.selector_kind,
            source.selector,
            source.declaring_yaml.display(),
            source.candidate_path.display(),
            source.resolved_path.display()
        ))
    })?;
    if resolved != source.resolved_path {
        return Err(StoreError::Harness(format!(
            "{} {} fixture `{}` selected by {} `{}` in `{}` changed canonical target from `{}` to `{}` after preparation",
            source.yaml_path,
            source.scope.as_str(),
            source.logical_path.display(),
            source.selector_kind,
            source.selector,
            source.declaring_yaml.display(),
            source.resolved_path.display(),
            resolved.display()
        )));
    }
    if !resolved.is_file() {
        return Err(StoreError::Harness(format!(
            "{} {} fixture `{}` selected by {} `{}` in `{}` resolved to `{}` which is no longer a regular file",
            source.yaml_path,
            source.scope.as_str(),
            source.logical_path.display(),
            source.selector_kind,
            source.selector,
            source.declaring_yaml.display(),
            resolved.display()
        )));
    }
    let sql = read_sql_file(&resolved).map_err(|error| {
        StoreError::Harness(format!(
            "{} {} fixture `{}` selected by {} `{}` in `{}` resolved to `{}`: could not read UTF-8 SQL file: {error}",
            source.yaml_path,
            source.scope.as_str(),
            source.logical_path.display(),
            source.selector_kind,
            source.selector,
            source.declaring_yaml.display(),
            resolved.display()
        ))
    })?;
    reject_transaction_control(&sql).map_err(|command| {
        StoreError::Harness(format!(
            "{} {} fixture `{}` selected by {} `{}` in `{}` resolved to `{}`: {}",
            source.yaml_path,
            source.scope.as_str(),
            source.logical_path.display(),
            source.selector_kind,
            source.selector,
            source.declaring_yaml.display(),
            resolved.display(),
            transaction_control_message(command)
        ))
    })?;
    Ok((resolved, sql))
}

fn legacy_suite_root_hint(selector: &str, context: &StoreDocContext) -> String {
    let selector_path = Path::new(selector);
    if !selector_is_relative(selector) || context.scope == StoreDocScope::Global {
        return String::new();
    }
    let legacy = context.suite_root.join(selector_path);
    if legacy.is_file() {
        format!(
            "; relative sql_file paths now resolve from the declaring YAML; the former suite-root location exists at `{}`",
            legacy.display()
        )
    } else {
        String::new()
    }
}

fn reject_foreign_absolute_path(selector: &str, yaml_path: &str) -> Result<(), ValidationError> {
    match classify_selector_path(selector) {
        SelectorPathKind::ForeignAbsolute(kind) => Err(ValidationError::new(
            yaml_path,
            format!(
                "{kind} absolute path `{selector}` cannot be resolved on this operating system"
            ),
        )),
        SelectorPathKind::Invalid(reason) => Err(ValidationError::new(
            yaml_path,
            format!("invalid fixture path `{selector}`: {reason}"),
        )),
        SelectorPathKind::NativeAbsolute | SelectorPathKind::Relative => Ok(()),
    }
}

#[derive(Clone, Copy)]
enum SelectorPathKind {
    NativeAbsolute,
    Relative,
    ForeignAbsolute(&'static str),
    Invalid(&'static str),
}

fn selector_is_relative(path: &str) -> bool {
    matches!(classify_selector_path(path), SelectorPathKind::Relative)
}

fn classify_selector_path(path: &str) -> SelectorPathKind {
    let bytes = path.as_bytes();
    let drive_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let drive_absolute = drive_prefix
        && bytes
            .get(2)
            .is_some_and(|separator| matches!(separator, b'/' | b'\\'));
    let unc = path.starts_with("//") || path.starts_with("\\\\");
    let unix_absolute = path.starts_with('/') && !unc;

    #[cfg(not(windows))]
    {
        if drive_absolute || unc {
            SelectorPathKind::ForeignAbsolute("Windows")
        } else if drive_prefix {
            SelectorPathKind::Invalid("Windows drive paths must be absolute")
        } else if unix_absolute {
            SelectorPathKind::NativeAbsolute
        } else {
            SelectorPathKind::Relative
        }
    }
    #[cfg(windows)]
    {
        if drive_absolute || unc {
            SelectorPathKind::NativeAbsolute
        } else if drive_prefix {
            SelectorPathKind::Invalid("Windows drive paths must be absolute")
        } else if unix_absolute {
            SelectorPathKind::ForeignAbsolute("Unix")
        } else if path.starts_with('\\') {
            SelectorPathKind::Invalid("rooted Windows paths require a drive or UNC share")
        } else {
            SelectorPathKind::Relative
        }
    }
}

fn utf8_path<'a>(
    path: &'a Path,
    yaml_path: &str,
    description: &str,
) -> Result<&'a str, ValidationError> {
    path.to_str().ok_or_else(|| {
        ValidationError::new(
            yaml_path,
            format!("{description} `{}` is not valid UTF-8", path.display()),
        )
    })
}

fn path_with_forward_slashes(path: &str) -> String {
    if std::path::MAIN_SEPARATOR == '/' {
        path.to_string()
    } else {
        path.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

fn path_sort_key(path: &Path) -> String {
    path_with_forward_slashes(&path.to_string_lossy())
}

impl PostgresStore {
    async fn fetch_rows(&self, table: &str) -> Result<Vec<Value>, StoreError> {
        let sql = format!(
            "SELECT to_jsonb(t) AS r FROM {} t LIMIT 1000",
            quote_ident(table)
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
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
                    format!(
                        "table `{table}`: expected row MISSING — {}",
                        identity_summary(exp)
                    ),
                    format!("{yaml_path}.expect[{i}]"),
                    exp.clone(),
                    FailureKind::MissingRow,
                );
                failure.near_misses = rank_near_misses(exp, rows, &claimed, ctx);
                out.checks.push(CheckResult::Fail(failure));
            }
        }
    }

    let count_mode = entry
        .get("count")
        .and_then(Value::as_str)
        .unwrap_or("at_least");
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
                    FailureKind::UnexpectedRow {
                        actual: row.clone(),
                    },
                )));
            }
        }
    }
}

fn row_matches(expected: &Value, actual: &Value, ctx: &MatchCtx) -> bool {
    let Some(exp) = expected.as_object() else {
        return false;
    };
    exp.iter()
        .all(|(col, matcher)| matchers::matches_value(matcher, actual.get(col), ctx))
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
            NearMiss {
                actual: row.clone(),
                diffs,
                score,
            }
        })
        .filter(|nm| nm.score >= 0.5)
        .collect();
    misses.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
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
    let col_list = cols
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
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
