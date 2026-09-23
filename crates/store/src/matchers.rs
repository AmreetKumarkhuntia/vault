//! The shared matcher vocabulary.
//!
//! An "expected" JSON value can be:
//! - a literal (deep equality),
//! - an operator map: `{"regex": ".."}`, `{"gt": 0}`, `{"one_of": [..]}`, …
//!   (all keys must be known operators; combined with AND),
//! - a tag matcher produced from YAML tags: `{"$tag": "uuid"}`,
//!   `{"$tag": "near-now", "arg": "5s"}`, …

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;

use crate::FieldDiff;

const OPERATOR_KEYS: &[&str] = &[
    "regex",
    "eq",
    "ne",
    "gt",
    "gte",
    "lt",
    "lte",
    "one_of",
    "len",
    "contains",
    "exists",
    "absent",
    "type",
    "json_partial",
    "starts_with",
    "ends_with",
    "under",
    "over",
];

#[derive(Debug, Clone, Copy, Default)]
pub struct MatchCtx {
    /// Anchor for `!near-now`, unix milliseconds (frozen per test at SEED).
    pub anchor_unix_ms: i64,
}

/// True when the value is an operator map (all keys are known operators).
pub fn is_matcher_map(v: &Value) -> bool {
    match v {
        Value::Object(m) if !m.is_empty() => m.keys().all(|k| OPERATOR_KEYS.contains(&k.as_str())),
        _ => false,
    }
}

pub fn is_tag_matcher(v: &Value) -> bool {
    matches!(v, Value::Object(m) if m.contains_key("$tag"))
}

/// Match one expected value against one (possibly absent) actual value.
pub fn matches_value(expected: &Value, actual: Option<&Value>, ctx: &MatchCtx) -> bool {
    if is_tag_matcher(expected) {
        return match_tag(expected, actual, ctx);
    }
    if is_matcher_map(expected) {
        return match_operators(expected, actual, ctx);
    }
    let Some(actual) = actual else { return false };
    match (expected, actual) {
        (Value::Object(_), Value::Object(_)) | (Value::Array(_), Value::Array(_)) => {
            deep_eq(expected, actual)
        }
        _ => scalar_eq(expected, actual),
    }
}

/// One-line human description of an expected value, for diff tables.
pub fn describe(expected: &Value) -> String {
    if let Some(m) = expected.as_object() {
        if let Some(tag) = m.get("$tag").and_then(|t| t.as_str()) {
            let arg = m
                .get("arg")
                .map(|a| format!(" {}", compact(a)))
                .unwrap_or_default();
            return match tag {
                "near-now" => format!(
                    "<within{} of test start>",
                    if arg.is_empty() { " 5s".into() } else { arg }
                ),
                other => format!("<{other}{arg}>"),
            };
        }
        if is_matcher_map(expected) {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{k} {}", compact(v)))
                .collect();
            return format!("<{}>", parts.join(", "));
        }
    }
    compact(expected)
}

fn compact(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn match_tag(expected: &Value, actual: Option<&Value>, ctx: &MatchCtx) -> bool {
    let m = expected.as_object().unwrap();
    let tag = m.get("$tag").and_then(|t| t.as_str()).unwrap_or("");
    let arg = m.get("arg");
    match tag {
        "absent" => actual.is_none(),
        "any" => actual.is_some(),
        _ => {
            let Some(actual) = actual else { return false };
            match tag {
                "null" => actual.is_null(),
                "not-null" => !actual.is_null(),
                "number" => {
                    actual.is_number() || actual.as_str().is_some_and(|s| s.parse::<f64>().is_ok())
                }
                "uuid" => actual.as_str().is_some_and(is_uuid),
                "iso8601" => actual
                    .as_str()
                    .is_some_and(|s| parse_timestamp(s).is_some()),
                "near-now" => {
                    let tol_ms = arg
                        .and_then(|a| a.as_str())
                        .and_then(humantime_ms)
                        .unwrap_or(5_000);
                    near_now(actual, ctx.anchor_unix_ms, tol_ms)
                }
                "json" => {
                    // containment: the actual (string or value) must contain arg
                    let Some(arg) = arg else { return false };
                    let actual_json = coerce_json(actual);
                    json_contains(arg, &actual_json, ctx).is_empty()
                }
                "one-of" => arg
                    .and_then(|a| a.as_array())
                    .is_some_and(|opts| opts.iter().any(|o| scalar_eq(o, actual))),
                _ => false,
            }
        }
    }
}

fn match_operators(expected: &Value, actual: Option<&Value>, ctx: &MatchCtx) -> bool {
    let m = expected.as_object().unwrap();
    for (op, arg) in m {
        let ok = match op.as_str() {
            "absent" => {
                let want_absent = arg.as_bool().unwrap_or(true);
                continue_if(want_absent == actual.is_none())
            }
            "exists" => {
                let want = arg.as_bool().unwrap_or(true);
                continue_if(want == actual.is_some())
            }
            _ => {
                let Some(actual) = actual else { return false };
                match op.as_str() {
                    "eq" => scalar_eq(arg, actual) || deep_eq(arg, actual),
                    "ne" => !(scalar_eq(arg, actual) || deep_eq(arg, actual)),
                    "regex" => regex_match(arg, actual),
                    "starts_with" => str_pair(arg, actual).is_some_and(|(a, b)| b.starts_with(&a)),
                    "ends_with" => str_pair(arg, actual).is_some_and(|(a, b)| b.ends_with(&a)),
                    "gt" => num_cmp(actual, arg).is_some_and(|o| o == std::cmp::Ordering::Greater),
                    "gte" => num_cmp(actual, arg).is_some_and(|o| o != std::cmp::Ordering::Less),
                    "lt" => num_cmp(actual, arg).is_some_and(|o| o == std::cmp::Ordering::Less),
                    "lte" => num_cmp(actual, arg).is_some_and(|o| o != std::cmp::Ordering::Greater),
                    "one_of" => arg
                        .as_array()
                        .is_some_and(|opts| opts.iter().any(|o| scalar_eq(o, actual))),
                    "len" => {
                        let len = match actual {
                            Value::String(s) => Some(s.chars().count() as u64),
                            Value::Array(a) => Some(a.len() as u64),
                            Value::Object(o) => Some(o.len() as u64),
                            _ => None,
                        };
                        len.is_some_and(|l| {
                            if is_matcher_map(arg) {
                                match_operators(arg, Some(&Value::from(l)), ctx)
                            } else {
                                arg.as_u64() == Some(l)
                            }
                        })
                    }
                    "contains" => match actual {
                        Value::String(s) => arg.as_str().is_some_and(|a| s.contains(a)),
                        Value::Array(items) => {
                            items.iter().any(|i| matches_value(arg, Some(i), ctx))
                        }
                        _ => false,
                    },
                    "type" => arg.as_str().is_some_and(|t| type_name(actual) == t),
                    "json_partial" => {
                        let actual_json = coerce_json(actual);
                        json_contains(arg, &actual_json, ctx).is_empty()
                    }
                    _ => false,
                }
            }
        };
        if !ok {
            return false;
        }
    }
    true
}

fn continue_if(b: bool) -> bool {
    b
}

fn str_pair(a: &Value, b: &Value) -> Option<(String, String)> {
    Some((a.as_str()?.to_string(), b.as_str()?.to_string()))
}

fn regex_match(pattern: &Value, actual: &Value) -> bool {
    let Some(p) = pattern.as_str() else {
        return false;
    };
    let hay = match actual {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    regex::Regex::new(p)
        .map(|re| re.is_match(&hay))
        .unwrap_or(false)
}

pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn scalar_eq(a: &Value, b: &Value) -> bool {
    if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
        return (x - y).abs() < f64::EPSILON * x.abs().max(y.abs()).max(1.0);
    }
    a == b
}

pub fn deep_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(ma), Value::Object(mb)) => {
            ma.len() == mb.len()
                && ma
                    .iter()
                    .all(|(k, v)| mb.get(k).is_some_and(|w| deep_eq(v, w)))
        }
        (Value::Array(xa), Value::Array(xb)) => {
            xa.len() == xb.len() && xa.iter().zip(xb).all(|(x, y)| deep_eq(x, y))
        }
        _ => scalar_eq(a, b),
    }
}

fn num_cmp(actual: &Value, arg: &Value) -> Option<std::cmp::Ordering> {
    let a = actual
        .as_f64()
        .or_else(|| actual.as_str().and_then(|s| s.parse().ok()))?;
    let b = arg.as_f64()?;
    a.partial_cmp(&b)
}

/// If the actual value is a string that parses as JSON, use the parsed value.
fn coerce_json(actual: &Value) -> Value {
    if let Value::String(s) = actual {
        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
            return parsed;
        }
    }
    actual.clone()
}

fn is_uuid(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, b) in bytes.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *b != b'-' {
                    return false;
                }
            }
            _ => {
                if !b.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

pub fn parse_timestamp(s: &str) -> Option<i64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(naive.and_utc().timestamp_millis());
        }
        if fmt == "%Y-%m-%d" {
            if let Ok(d) = chrono::NaiveDate::parse_from_str(s, fmt) {
                return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
            }
        }
    }
    None
}

fn near_now(actual: &Value, anchor_ms: i64, tol_ms: i64) -> bool {
    let actual_ms = match actual {
        Value::String(s) => parse_timestamp(s),
        Value::Number(n) => n.as_i64().map(|secs_or_ms| {
            // Heuristic: values below 10^12 are seconds.
            if secs_or_ms < 1_000_000_000_000 {
                secs_or_ms * 1000
            } else {
                secs_or_ms
            }
        }),
        _ => None,
    };
    let anchor = if anchor_ms == 0 {
        Utc::now().timestamp_millis()
    } else {
        anchor_ms
    };
    actual_ms.is_some_and(|a| (a - anchor).abs() <= tol_ms)
}

pub fn humantime_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len()));
    let n: f64 = num.trim().parse().ok()?;
    let mult = match unit.trim() {
        "ms" => 1.0,
        "s" | "sec" | "" => 1000.0,
        "m" | "min" => 60_000.0,
        "h" => 3_600_000.0,
        _ => return None,
    };
    Some((n * mult) as i64)
}

/// Containment check ("json_partial"): every leaf in `expected` must be
/// satisfied somewhere in `actual`. Returns field-level diffs; empty = match.
pub fn json_contains(expected: &Value, actual: &Value, ctx: &MatchCtx) -> Vec<FieldDiff> {
    let mut diffs = Vec::new();
    contains_at(expected, Some(actual), "$", ctx, &mut diffs);
    diffs
}

fn contains_at(
    expected: &Value,
    actual: Option<&Value>,
    path: &str,
    ctx: &MatchCtx,
    diffs: &mut Vec<FieldDiff>,
) {
    if is_tag_matcher(expected) || is_matcher_map(expected) {
        if !matches_value(expected, actual, ctx) {
            diffs.push(FieldDiff {
                path: path.to_string(),
                expected: expected.clone(),
                actual: actual.cloned().unwrap_or(Value::Null),
            });
        }
        return;
    }
    match expected {
        Value::Object(exp_map) => match actual.and_then(|a| a.as_object()) {
            Some(act_map) => {
                for (k, v) in exp_map {
                    contains_at(v, act_map.get(k), &format!("{path}.{k}"), ctx, diffs);
                }
            }
            None => diffs.push(FieldDiff {
                path: path.to_string(),
                expected: expected.clone(),
                actual: actual.cloned().unwrap_or(Value::Null),
            }),
        },
        Value::Array(exp_items) => match actual.and_then(|a| a.as_array()) {
            Some(act_items) => {
                // Unordered subset: each expected element must match a distinct actual one.
                let mut used = vec![false; act_items.len()];
                for (i, exp) in exp_items.iter().enumerate() {
                    let found = act_items.iter().enumerate().find(|(j, act)| {
                        !used[*j] && {
                            let mut sub = Vec::new();
                            contains_at(exp, Some(act), path, ctx, &mut sub);
                            sub.is_empty()
                        }
                    });
                    match found {
                        Some((j, _)) => used[j] = true,
                        None => diffs.push(FieldDiff {
                            path: format!("{path}[{i}]"),
                            expected: exp.clone(),
                            actual: Value::String("<no matching element>".into()),
                        }),
                    }
                }
            }
            None => diffs.push(FieldDiff {
                path: path.to_string(),
                expected: expected.clone(),
                actual: actual.cloned().unwrap_or(Value::Null),
            }),
        },
        _ => {
            if !matches_value(expected, actual, ctx) {
                diffs.push(FieldDiff {
                    path: path.to_string(),
                    expected: expected.clone(),
                    actual: actual.cloned().unwrap_or(Value::Null),
                });
            }
        }
    }
}

/// Exact deep equality with an ignore-list of paths ("$.created_at").
pub fn json_exact(
    expected: &Value,
    actual: &Value,
    ignore: &[String],
    ctx: &MatchCtx,
) -> Vec<FieldDiff> {
    let mut diffs = Vec::new();
    exact_at(expected, Some(actual), "$", ignore, ctx, &mut diffs);
    diffs
}

fn exact_at(
    expected: &Value,
    actual: Option<&Value>,
    path: &str,
    ignore: &[String],
    ctx: &MatchCtx,
    diffs: &mut Vec<FieldDiff>,
) {
    if ignore.iter().any(|p| p == path) {
        return;
    }
    if is_tag_matcher(expected) || is_matcher_map(expected) {
        if !matches_value(expected, actual, ctx) {
            diffs.push(FieldDiff {
                path: path.into(),
                expected: expected.clone(),
                actual: actual.cloned().unwrap_or(Value::Null),
            });
        }
        return;
    }
    match (expected, actual) {
        (Value::Object(em), Some(Value::Object(am))) => {
            for (k, v) in em {
                exact_at(v, am.get(k), &format!("{path}.{k}"), ignore, ctx, diffs);
            }
            for k in am.keys() {
                if !em.contains_key(k) && !ignore.iter().any(|p| p == &format!("{path}.{k}")) {
                    diffs.push(FieldDiff {
                        path: format!("{path}.{k}"),
                        expected: Value::String("<absent>".into()),
                        actual: am[k].clone(),
                    });
                }
            }
        }
        (Value::Array(ea), Some(Value::Array(aa))) => {
            if ea.len() != aa.len() {
                diffs.push(FieldDiff {
                    path: path.into(),
                    expected: Value::String(format!("<array of {}>", ea.len())),
                    actual: Value::String(format!("<array of {}>", aa.len())),
                });
            }
            for (i, e) in ea.iter().enumerate() {
                exact_at(e, aa.get(i), &format!("{path}[{i}]"), ignore, ctx, diffs);
            }
        }
        _ => {
            if !matches_value(expected, actual, ctx) {
                diffs.push(FieldDiff {
                    path: path.into(),
                    expected: expected.clone(),
                    actual: actual.cloned().unwrap_or(Value::Null),
                });
            }
        }
    }
}

/// Score how closely a flat expected object matches an actual object:
/// (matched fields / total fields, per-field diffs). Used for near-miss ranking.
pub fn score_object(expected: &Value, actual: &Value, ctx: &MatchCtx) -> (f32, Vec<FieldDiff>) {
    let Some(exp) = expected.as_object() else {
        return (0.0, vec![]);
    };
    let act = actual.as_object();
    let total = exp.len().max(1);
    let mut matched = 0usize;
    let mut diffs = Vec::new();
    for (k, v) in exp {
        let actual_v = act.and_then(|m| m.get(k));
        if matches_value(v, actual_v, ctx) {
            matched += 1;
        } else {
            diffs.push(FieldDiff {
                path: k.clone(),
                expected: v.clone(),
                actual: actual_v.cloned().unwrap_or(Value::Null),
            });
        }
    }
    (matched as f32 / total as f32, diffs)
}

/// Kuhn's augmenting-path maximum bipartite matching.
/// `adj[i]` = right-side indices expectation `i` can match.
/// Returns `match_left[i] = Some(j)` assignments.
pub fn max_bipartite(n_right: usize, adj: &[Vec<usize>]) -> Vec<Option<usize>> {
    let n_left = adj.len();
    let mut match_right: Vec<Option<usize>> = vec![None; n_right];

    fn try_kuhn(
        u: usize,
        adj: &[Vec<usize>],
        visited: &mut [bool],
        match_right: &mut [Option<usize>],
    ) -> bool {
        for &v in &adj[u] {
            if !visited[v] {
                visited[v] = true;
                if match_right[v].is_none()
                    || try_kuhn(match_right[v].unwrap(), adj, visited, match_right)
                {
                    match_right[v] = Some(u);
                    return true;
                }
            }
        }
        false
    }

    for u in 0..n_left {
        let mut visited = vec![false; n_right];
        try_kuhn(u, adj, &mut visited, &mut match_right);
    }

    let mut match_left = vec![None; n_left];
    for (j, m) in match_right.iter().enumerate() {
        if let Some(i) = m {
            match_left[*i] = Some(j);
        }
    }
    match_left
}
