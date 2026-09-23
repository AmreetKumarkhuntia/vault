use serde_json::Value;
use vault_dsl::MatchSpec;
use vault_store::matchers::{self, MatchCtx};

use crate::record::RecordedRequest;

/// Does a request satisfy a `match:` spec? Used both at serve time (stub
/// selection) and at verify time (`verify.calls`).
pub fn request_matches(spec: &MatchSpec, req: &RecordedRequest) -> bool {
    component_score(spec, req) == full_score(spec)
}

/// Weighted similarity used for near-miss ranking:
/// path 3, method 2, body 2, query 1, headers 1.
pub fn component_score(spec: &MatchSpec, req: &RecordedRequest) -> u32 {
    let ctx = MatchCtx::default();
    let mut score = 0;
    if let Some(m) = &spec.method {
        if m.eq_ignore_ascii_case(&req.method) {
            score += 2;
        }
    }
    if let Some(p) = &spec.path {
        if path_matches(p, &req.path).is_some() {
            score += 3;
        }
    }
    if !spec.query.is_empty() {
        let all = spec.query.iter().all(|(k, v)| {
            let actual = req.query.get(k);
            match v.as_str() {
                Some("*") => actual.is_some(),
                _ => actual.is_some_and(|a| loose_eq(v, a)),
            }
        });
        if all {
            score += 1;
        }
    }
    if !spec.headers.is_empty() {
        let hdrs = req.headers.as_object();
        let all = spec.headers.iter().all(|(k, v)| {
            let actual = hdrs.and_then(|h| {
                h.iter()
                    .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
                    .map(|(_, hv)| hv)
            });
            match v.as_str() {
                Some("*") => actual.is_some(),
                _ => actual.is_some_and(|a| matchers::matches_value(v, Some(a), &ctx)),
            }
        });
        if all {
            score += 1;
        }
    }
    if let Some(body) = &spec.body {
        if body_matches(body, req, &ctx) {
            score += 2;
        }
    }
    score
}

/// The score a fully-matching request would earn for this spec.
pub fn full_score(spec: &MatchSpec) -> u32 {
    let mut total = 0;
    if spec.method.is_some() {
        total += 2;
    }
    if spec.path.is_some() {
        total += 3;
    }
    if !spec.query.is_empty() {
        total += 1;
    }
    if !spec.headers.is_empty() {
        total += 1;
    }
    if spec.body.is_some() {
        total += 2;
    }
    total
}

/// Match a path spec against an actual path. Returns captured `{param}`
/// values on success (empty map for exact/regex matches).
pub fn path_matches(spec: &Value, actual: &str) -> Option<Value> {
    let actual = actual.split('?').next().unwrap_or(actual);
    match spec {
        Value::String(pattern) => {
            if let Some(prefix) = pattern.strip_suffix("/**") {
                return actual
                    .starts_with(prefix)
                    .then(|| Value::Object(Default::default()));
            }
            let pat_segs: Vec<&str> = pattern.trim_matches('/').split('/').collect();
            let act_segs: Vec<&str> = actual.trim_matches('/').split('/').collect();
            if pat_segs.len() != act_segs.len() {
                return None;
            }
            let mut params = serde_json::Map::new();
            for (p, a) in pat_segs.iter().zip(&act_segs) {
                if let Some(name) = p.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                    params.insert(name.to_string(), Value::String((*a).to_string()));
                } else if p != a {
                    return None;
                }
            }
            Some(Value::Object(params))
        }
        Value::Object(m) => {
            let re = m.get("regex")?.as_str()?;
            regex::Regex::new(re)
                .ok()?
                .is_match(actual)
                .then(|| Value::Object(Default::default()))
        }
        _ => None,
    }
}

/// Body matchers: `{json_partial: ..}` | `{json_exact: ..}` | `{regex: ..}`
/// | a literal object (treated as json_partial).
pub fn body_matches(spec: &Value, req: &RecordedRequest, ctx: &MatchCtx) -> bool {
    if let Some(m) = spec.as_object() {
        if let Some(partial) = m.get("json_partial") {
            return matchers::json_contains(partial, &req.body_json, ctx).is_empty();
        }
        if let Some(exact) = m.get("json_exact") {
            return matchers::json_exact(exact, &req.body_json, &[], ctx).is_empty();
        }
        if let Some(re) = m.get("regex").and_then(|r| r.as_str()) {
            return regex::Regex::new(re)
                .map(|r| r.is_match(&req.body))
                .unwrap_or(false);
        }
        // Literal object: containment semantics.
        return matchers::json_contains(spec, &req.body_json, ctx).is_empty();
    }
    if let Some(s) = spec.as_str() {
        return req.body == s;
    }
    false
}

fn loose_eq(expected: &Value, actual: &Value) -> bool {
    if expected == actual {
        return true;
    }
    // Query params arrive as strings; compare textually.
    let e = expected
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| expected.to_string());
    let a = actual
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| actual.to_string());
    e == a
}
