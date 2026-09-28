//! Postgres driver: seeding, TRUNCATE isolation, and expectation-driven
//! verification with near-miss reporting.

use std::path::{Component, Path, PathBuf};
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
        self.validate_impl(doc, mode, None)
    }

    fn validate_with_suite_root(
        &self,
        doc: &StoreDoc,
        mode: DocMode,
        suite_root: &Path,
    ) -> Result<(), ValidationError> {
        self.validate_impl(doc, mode, Some(suite_root))
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
    fn validate_impl(
        &self,
        doc: &StoreDoc,
        mode: DocMode,
        suite_root: Option<&Path>,
    ) -> Result<(), ValidationError> {
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

                    let variants: Vec<&str> = ["table", "sql", "sql_file"]
                        .into_iter()
                        .filter(|key| m.contains_key(*key))
                        .collect();
                    if variants.len() != 1 {
                        return Err(ValidationError::new(
                            &path,
                            "needs exactly one of `table`, `sql`, or `sql_file`",
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

                            if let Some(root) = suite_root {
                                let resolved =
                                    resolve_sql_file(root, logical).map_err(|error| {
                                        ValidationError::new(
                                            format!("{path}.sql_file"),
                                            format!(
                                                "`{logical}` resolved as `{}`: {}",
                                                error.resolved_path.display(),
                                                error.message
                                            ),
                                        )
                                    })?;
                                let sql = read_sql_file(&resolved).map_err(|e| {
                                    ValidationError::new(
                                        format!("{path}.sql_file"),
                                        format!(
                                            "`{logical}` resolved to `{}`: could not read UTF-8 SQL file: {e}",
                                            resolved.display()
                                        ),
                                    )
                                })?;
                                reject_transaction_control(&sql).map_err(|command| {
                                    ValidationError::new(
                                        format!("{path}.sql_file"),
                                        format!(
                                            "`{logical}` resolved to `{}`: {}",
                                            resolved.display(),
                                            transaction_control_message(command)
                                        ),
                                    )
                                })?;
                            } else {
                                validate_sql_file_reference(logical).map_err(|message| {
                                    ValidationError::new(
                                        format!("{path}.sql_file"),
                                        format!("`{logical}`: {message}"),
                                    )
                                })?;
                            }
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
        PostgresDriver
            .validate_with_suite_root(doc, DocMode::Seed, &self.suite_root)
            .map_err(|e| StoreError::Harness(e.to_string()))?;
        let entries = doc
            .as_array()
            .ok_or_else(|| StoreError::Harness("seed.postgres must be a list".into()))?;
        let mut receipt = SeedReceipt::default();
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        let execution: Result<(), StoreError> = async {
            for (index, entry) in entries.iter().enumerate() {
                if let Some(sql) = entry.get("sql").and_then(Value::as_str) {
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
                } else if let Some(file) = entry.get("sql_file").and_then(Value::as_str) {
                    let path = resolve_sql_file(&self.suite_root, file).map_err(|error| {
                        StoreError::Harness(format!(
                            "seed.postgres[{index}].sql_file `{file}` resolved as `{}`: {}",
                            error.resolved_path.display(),
                            error.message
                        ))
                    })?;
                    let sql = read_sql_file(&path).map_err(|e| {
                        StoreError::Harness(format!(
                            "seed.postgres[{index}].sql_file `{file}` resolved to `{}`: could not read UTF-8 SQL file: {e}",
                            path.display()
                        ))
                    })?;
                    reject_transaction_control(&sql).map_err(|command| {
                        StoreError::Harness(format!(
                            "seed.postgres[{index}].sql_file `{file}` resolved to `{}`: {}",
                            path.display(),
                            transaction_control_message(command)
                        ))
                    })?;
                    set_standard_conforming_strings(&mut tx)
                        .await
                        .map_err(|e| {
                            StoreError::Harness(format!(
                                "seed.postgres[{index}].sql_file `{file}` resolved to `{}`: could not enforce standard string parsing: {e}",
                                path.display()
                            ))
                        })?;
                    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| {
                            StoreError::Harness(format!(
                                "seed.postgres[{index}].sql_file `{file}` resolved to `{}`: postgres: {e}",
                                path.display()
                            ))
                        })?;
                    receipt.entries.push(format!("postgres: executed {file}"));
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
        "SET"
            if token_is(tokens, 1, "SESSION")
                && token_is(tokens, 2, "CHARACTERISTICS")
                && token_is(tokens, 3, "AS")
                && token_is(tokens, 4, "TRANSACTION") =>
        {
            Some("SET SESSION CHARACTERISTICS AS TRANSACTION")
        }
        _ => None,
    }
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

#[derive(Debug)]
struct SqlFileResolutionError {
    resolved_path: PathBuf,
    message: String,
}

fn sql_file_resolution_error(
    resolved_path: impl Into<PathBuf>,
    message: impl Into<String>,
) -> SqlFileResolutionError {
    SqlFileResolutionError {
        resolved_path: resolved_path.into(),
        message: message.into(),
    }
}

fn validate_sql_file_reference(logical_path: &str) -> Result<(), String> {
    let relative = Path::new(logical_path);
    if logical_path.contains("{{") || logical_path.contains("}}") {
        return Err("must be a static path without template expressions".into());
    }
    if relative.is_absolute() {
        return Err("must be relative to the suite root".into());
    }
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("must not contain parent, root, or platform-prefix components".into());
    }
    if relative
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("sql")
    {
        return Err("must have a `.sql` extension".into());
    }
    Ok(())
}

fn resolve_sql_file(
    suite_root: &Path,
    logical_path: &str,
) -> Result<PathBuf, SqlFileResolutionError> {
    let relative = Path::new(logical_path);
    let candidate = suite_root.join(relative);

    validate_sql_file_reference(logical_path)
        .map_err(|message| sql_file_resolution_error(&candidate, message))?;

    let canonical_root = std::fs::canonicalize(suite_root).map_err(|e| {
        sql_file_resolution_error(
            suite_root,
            format!("could not canonicalize suite root: {e}"),
        )
    })?;
    let resolved = std::fs::canonicalize(&candidate).map_err(|e| {
        sql_file_resolution_error(&candidate, format!("could not resolve SQL file: {e}"))
    })?;
    if !resolved.starts_with(&canonical_root) {
        return Err(sql_file_resolution_error(
            resolved,
            "resolves outside the suite root",
        ));
    }
    if !resolved.is_file() {
        return Err(sql_file_resolution_error(
            resolved,
            "must resolve to a regular file",
        ));
    }

    Ok(resolved)
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
