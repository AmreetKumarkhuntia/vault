//! Redis driver: seeding, FLUSHDB isolation, typed key verification.

use std::sync::Arc;

use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde_json::{json, Value};
use vault_store::matchers::{self, MatchCtx};
use vault_store::*;

pub struct RedisDriver;

#[async_trait]
impl StoreDriver for RedisDriver {
    fn kind(&self) -> &'static str {
        "redis"
    }

    fn validate(&self, doc: &StoreDoc, mode: DocMode) -> Result<(), ValidationError> {
        let (root, need) = match mode {
            DocMode::Seed => ("seed.redis", &["set", "hash", "list", "zset", "sadd"][..]),
            DocMode::Verify => ("verify.redis", &[][..]),
            _ => return Ok(()),
        };
        let entries = doc
            .as_array()
            .ok_or_else(|| ValidationError::new(root, "must be a list of entries"))?;
        for (i, e) in entries.iter().enumerate() {
            let path = format!("{root}[{i}]");
            let m = e
                .as_object()
                .ok_or_else(|| ValidationError::new(&path, "must be a mapping"))?;
            if mode == DocMode::Seed && !m.keys().any(|k| need.contains(&k.as_str())) {
                return Err(ValidationError::new(
                    &path,
                    "needs one of set/hash/list/zset/sadd",
                ));
            }
            if mode == DocMode::Verify && !m.contains_key("key") && !m.contains_key("pattern") {
                return Err(ValidationError::new(&path, "needs `key` or `pattern`"));
            }
        }
        Ok(())
    }

    async fn connect(&self, cfg: &StoreConnConfig) -> Result<Arc<dyn StateStore>, StoreError> {
        let client = redis::Client::open(cfg.url.as_str())
            .map_err(|e| StoreError::Connection(format!("redis: {e}")))?;
        // FLUSHDB on the default database would wipe unrelated local state;
        // a dedicated logical db (e.g. /15) is the isolation contract.
        let db_index = cfg
            .url
            .rsplit('/')
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        let allow_db0 = cfg
            .options
            .get("allow_db0")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if db_index == 0 && !allow_db0 {
            return Err(StoreError::Connection(
                "redis: refusing db 0 — point the URL at a dedicated logical db (e.g. redis://host:6379/15) or set allow_db0: true".into(),
            ));
        }
        let manager = ConnectionManager::new(client)
            .await
            .map_err(|e| StoreError::Connection(format!("redis: {e}")))?;
        Ok(Arc::new(RedisStore {
            alias: cfg.alias.clone(),
            conn: manager,
        }))
    }
}

pub struct RedisStore {
    alias: String,
    conn: ConnectionManager,
}

#[async_trait]
impl StateStore for RedisStore {
    fn kind(&self) -> &'static str {
        "redis"
    }
    fn alias(&self) -> &str {
        &self.alias
    }

    async fn ping(&self) -> Result<(), StoreError> {
        let mut c = self.conn.clone();
        redis::cmd("PING")
            .query_async::<String>(&mut c)
            .await
            .map(|_| ())
            .map_err(rd_err)
    }

    async fn reset(&self, spec: &StoreDoc) -> Result<(), StoreError> {
        let mut c = self.conn.clone();
        let mode = spec
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("flushdb");
        match mode {
            "none" => Ok(()),
            "scan" => {
                let prefixes: Vec<String> = spec
                    .get("prefixes")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                for prefix in prefixes {
                    let keys: Vec<String> = scan_keys(&mut c, &format!("{prefix}*")).await?;
                    if !keys.is_empty() {
                        c.del::<_, ()>(keys).await.map_err(rd_err)?;
                    }
                }
                Ok(())
            }
            _ => redis::cmd("FLUSHDB")
                .arg("ASYNC")
                .query_async::<String>(&mut c)
                .await
                .map(|_| ())
                .map_err(rd_err),
        }
    }

    async fn seed(&self, doc: &StoreDoc) -> Result<SeedReceipt, StoreError> {
        let entries = doc
            .as_array()
            .ok_or_else(|| StoreError::Harness("seed.redis must be a list".into()))?;
        let mut c = self.conn.clone();
        let mut receipt = SeedReceipt::default();

        for entry in entries {
            if let Some(set) = entry.get("set") {
                let key = req_str(set, "key")?;
                let value = match (set.get("value"), set.get("json")) {
                    (Some(v), _) => as_text(v),
                    (None, Some(j)) => j.to_string(),
                    (None, None) => {
                        return Err(StoreError::Harness(format!(
                            "seed redis `{key}`: needs value or json"
                        )))
                    }
                };
                match set.get("ttl").and_then(Value::as_i64) {
                    Some(ttl) => c
                        .set_ex::<_, _, ()>(&key, value, ttl as u64)
                        .await
                        .map_err(rd_err)?,
                    None => c.set::<_, _, ()>(&key, value).await.map_err(rd_err)?,
                }
                receipt.entries.push(format!("redis: SET {key}"));
            } else if let Some(hash) = entry.get("hash") {
                let key = req_str(hash, "key")?;
                let fields = hash
                    .get("fields")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        StoreError::Harness(format!("seed redis hash `{key}`: fields missing"))
                    })?;
                let pairs: Vec<(String, String)> = fields
                    .iter()
                    .map(|(k, v)| (k.clone(), as_text(v)))
                    .collect();
                c.hset_multiple::<_, _, _, ()>(&key, &pairs)
                    .await
                    .map_err(rd_err)?;
                receipt.entries.push(format!("redis: HSET {key}"));
            } else if let Some(list) = entry.get("list") {
                let key = req_str(list, "key")?;
                let values: Vec<String> = list
                    .get("values")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(as_text).collect())
                    .unwrap_or_default();
                c.rpush::<_, _, ()>(&key, values).await.map_err(rd_err)?;
                receipt.entries.push(format!("redis: RPUSH {key}"));
            } else if let Some(zset) = entry.get("zset") {
                let key = req_str(zset, "key")?;
                let members = zset
                    .get("members")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        StoreError::Harness(format!("seed redis zset `{key}`: members missing"))
                    })?;
                for (member, score) in members {
                    let score = score.as_f64().unwrap_or(0.0);
                    c.zadd::<_, _, _, ()>(&key, member, score)
                        .await
                        .map_err(rd_err)?;
                }
                receipt.entries.push(format!("redis: ZADD {key}"));
            } else if let Some(sadd) = entry.get("sadd") {
                let key = req_str(sadd, "key")?;
                let members: Vec<String> = sadd
                    .get("members")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(as_text).collect())
                    .unwrap_or_default();
                c.sadd::<_, _, ()>(&key, members).await.map_err(rd_err)?;
                receipt.entries.push(format!("redis: SADD {key}"));
            }
        }
        Ok(receipt)
    }

    async fn snapshot(&self, _doc: &StoreDoc) -> Result<Snapshot, StoreError> {
        Ok(Value::Null)
    }

    async fn diff_snapshot(
        &self,
        _doc: &StoreDoc,
        _before: &Snapshot,
    ) -> Result<VerifyOutcome, StoreError> {
        Ok(VerifyOutcome::default())
    }

    async fn verify(&self, doc: &StoreDoc, opts: &VerifyOpts) -> Result<VerifyOutcome, StoreError> {
        let ctx = MatchCtx {
            anchor_unix_ms: opts.anchor_unix_ms,
        };
        let entries = doc
            .as_array()
            .ok_or_else(|| StoreError::Harness("verify.redis must be a list".into()))?;
        let mut c = self.conn.clone();
        let mut out = VerifyOutcome::default();

        for (ei, entry) in entries.iter().enumerate() {
            let yaml_path = format!("verify.redis[{ei}]");
            if let Some(pattern) = entry.get("pattern").and_then(Value::as_str) {
                let keys = scan_keys(&mut c, pattern).await?;
                let expected_count = entry.get("count").cloned().unwrap_or(json!({"gte": 1}));
                if matchers::matches_value(&expected_count, Some(&json!(keys.len())), &ctx) {
                    out.push_pass(format!("redis: pattern {pattern} → {} keys", keys.len()));
                } else {
                    out.checks.push(CheckResult::Fail(CheckFailure::new(
                        format!("redis: pattern `{pattern}` has {} keys", keys.len()),
                        yaml_path,
                        json!({"pattern": pattern, "count": expected_count}),
                        FailureKind::CountMismatch {
                            expected: expected_count.to_string(),
                            actual: keys.len() as u64,
                        },
                    )));
                }
                continue;
            }

            let Some(key) = entry.get("key").and_then(Value::as_str) else {
                continue;
            };
            let exists: bool = c.exists(key).await.map_err(rd_err)?;

            if entry.get("absent").and_then(Value::as_bool) == Some(true) {
                if exists {
                    let actual = read_key(&mut c, key).await?;
                    out.checks.push(CheckResult::Fail(CheckFailure::new(
                        format!("redis: key `{key}` expected absent but exists"),
                        yaml_path,
                        json!({"absent": true}),
                        FailureKind::UnexpectedKey {
                            key: key.to_string(),
                        },
                    )));
                    let _ = actual;
                } else {
                    out.push_pass(format!("redis: {key} absent"));
                }
                continue;
            }

            if !exists {
                out.checks.push(CheckResult::Fail(CheckFailure::new(
                    format!("redis: key `{key}` MISSING"),
                    yaml_path,
                    entry.clone(),
                    FailureKind::MissingKey,
                )));
                continue;
            }

            let actual = read_key(&mut c, key).await?;
            let mut diffs = Vec::new();

            if let Some(expected_type) = entry.get("type").and_then(Value::as_str) {
                let actual_type: String = redis::cmd("TYPE")
                    .arg(key)
                    .query_async(&mut c)
                    .await
                    .map_err(rd_err)?;
                if actual_type != expected_type {
                    diffs.push(FieldDiff {
                        path: "type".into(),
                        expected: json!(expected_type),
                        actual: json!(actual_type),
                    });
                }
            }
            if let Some(value) = entry.get("value") {
                if !matchers::matches_value(value, Some(&actual), &ctx) {
                    diffs.push(FieldDiff {
                        path: "value".into(),
                        expected: value.clone(),
                        actual: actual.clone(),
                    });
                }
            }
            if let Some(partial) = entry.get("json_partial") {
                let parsed = actual
                    .as_str()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or_else(|| actual.clone());
                diffs.extend(matchers::json_contains(partial, &parsed, &ctx));
            }
            if let Some(re) = entry.get("regex").and_then(Value::as_str) {
                let text = actual
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| actual.to_string());
                let ok = regex::Regex::new(re)
                    .map(|r| r.is_match(&text))
                    .unwrap_or(false);
                if !ok {
                    diffs.push(FieldDiff {
                        path: "value".into(),
                        expected: json!({"regex": re}),
                        actual: actual.clone(),
                    });
                }
            }
            if let Some(ttl_matcher) = entry.get("ttl") {
                let ttl: i64 = c.ttl(key).await.map_err(rd_err)?;
                if !matchers::matches_value(ttl_matcher, Some(&json!(ttl)), &ctx) {
                    diffs.push(FieldDiff {
                        path: "ttl".into(),
                        expected: ttl_matcher.clone(),
                        actual: json!(ttl),
                    });
                }
            }

            if diffs.is_empty() {
                out.push_pass(format!("redis: {key}"));
            } else {
                out.checks.push(CheckResult::Fail(CheckFailure::new(
                    format!("redis: key `{key}` mismatch"),
                    yaml_path,
                    entry.clone(),
                    FailureKind::ValueMismatch { diffs },
                )));
            }
        }
        Ok(out)
    }

    async fn inspect(&self, query: &str) -> Result<Table, StoreError> {
        let parts: Vec<&str> = query.split_whitespace().collect();
        let allowed = [
            "GET", "TYPE", "TTL", "HGETALL", "LRANGE", "SMEMBERS", "KEYS", "EXISTS", "ZRANGE",
            "SCARD", "LLEN",
        ];
        let Some(cmd) = parts.first() else {
            return Err(StoreError::Harness("empty redis command".into()));
        };
        if !allowed.contains(&cmd.to_uppercase().as_str()) {
            return Err(StoreError::Harness(format!(
                "redis inspect allows read-only commands only: {}",
                allowed.join(", ")
            )));
        }
        let mut c = self.conn.clone();
        let mut command = redis::cmd(&cmd.to_uppercase());
        for arg in &parts[1..] {
            command.arg(*arg);
        }
        let raw: redis::Value = command.query_async(&mut c).await.map_err(rd_err)?;
        Ok(Table {
            columns: vec!["result".into()],
            rows: vec![vec![redis_to_json(&raw)]],
        })
    }
}

async fn scan_keys(c: &mut ConnectionManager, pattern: &str) -> Result<Vec<String>, StoreError> {
    let mut cursor = 0u64;
    let mut keys = Vec::new();
    loop {
        let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(200)
            .query_async(c)
            .await
            .map_err(rd_err)?;
        keys.extend(batch);
        if next == 0 {
            return Ok(keys);
        }
        cursor = next;
    }
}

async fn read_key(c: &mut ConnectionManager, key: &str) -> Result<Value, StoreError> {
    let key_type: String = redis::cmd("TYPE")
        .arg(key)
        .query_async(c)
        .await
        .map_err(rd_err)?;
    let v = match key_type.as_str() {
        "string" => json!(c.get::<_, String>(key).await.map_err(rd_err)?),
        "hash" => {
            let map: std::collections::HashMap<String, String> =
                c.hgetall(key).await.map_err(rd_err)?;
            json!(map)
        }
        "list" => json!(c
            .lrange::<_, Vec<String>>(key, 0, -1)
            .await
            .map_err(rd_err)?),
        "set" => {
            let mut members: Vec<String> = c.smembers(key).await.map_err(rd_err)?;
            members.sort();
            json!(members)
        }
        "zset" => {
            let members: Vec<(String, f64)> =
                c.zrange_withscores(key, 0, -1).await.map_err(rd_err)?;
            Value::Object(members.into_iter().map(|(m, s)| (m, json!(s))).collect())
        }
        other => json!(format!("<unsupported type {other}>")),
    };
    Ok(v)
}

fn redis_to_json(v: &redis::Value) -> Value {
    match v {
        redis::Value::Nil => Value::Null,
        redis::Value::Int(i) => json!(i),
        redis::Value::BulkString(bytes) => json!(String::from_utf8_lossy(bytes).to_string()),
        redis::Value::SimpleString(s) => json!(s),
        redis::Value::Array(items) => Value::Array(items.iter().map(redis_to_json).collect()),
        other => json!(format!("{other:?}")),
    }
}

fn req_str(v: &Value, field: &str) -> Result<String, StoreError> {
    v.get(field)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| StoreError::Harness(format!("seed redis entry missing `{field}`")))
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn rd_err(e: redis::RedisError) -> StoreError {
    StoreError::Harness(format!("redis: {e}"))
}
