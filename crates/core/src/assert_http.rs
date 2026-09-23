use serde_json::{json, Value};
use serde_json_path::JsonPath;
use vault_dsl::ExpectSpec;
use vault_store::matchers::{self, MatchCtx};
use vault_store::{CheckFailure, CheckResult, FailureKind, VerifyOutcome};

use crate::http::StepResponse;

pub fn assert_response(
    step: &str,
    spec: &ExpectSpec,
    resp: &StepResponse,
    ctx: &MatchCtx,
) -> VerifyOutcome {
    let mut out = VerifyOutcome::default();
    let at = |field: &str| format!("steps.{step}.expect.{field}");

    if let Some(status_spec) = &spec.status {
        if status_ok(status_spec, resp.status) {
            out.push_pass(format!("{step}: status {}", resp.status));
        } else {
            out.checks.push(fail(
                format!(
                    "{step}: status is {} — expected {}",
                    resp.status,
                    compact(status_spec)
                ),
                at("status"),
                status_spec.clone(),
                vec![diff("status", status_spec.clone(), json!(resp.status))],
            ));
        }
    }

    for (name, matcher) in &spec.headers {
        let actual = resp
            .headers
            .as_object()
            .and_then(|h| h.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)))
            .map(|(_, v)| v);
        let ok = match matcher.as_str() {
            Some("absent") => actual.is_none(),
            _ => matchers::matches_value(matcher, actual, ctx),
        };
        if ok {
            out.push_pass(format!("{step}: header {name}"));
        } else {
            out.checks.push(fail(
                format!("{step}: header `{name}` mismatch"),
                at(&format!("headers.{name}")),
                matcher.clone(),
                vec![diff(
                    name,
                    matcher.clone(),
                    actual.cloned().unwrap_or(Value::Null),
                )],
            ));
        }
    }

    if let Some(partial) = &spec.json_partial {
        let diffs = matchers::json_contains(partial, &resp.body_json, ctx);
        if diffs.is_empty() {
            out.push_pass(format!("{step}: json_partial"));
        } else {
            out.checks.push(with_diffs(
                format!("{step}: response body does not contain expected JSON"),
                at("json_partial"),
                partial.clone(),
                diffs,
                &resp.body_json,
            ));
        }
    }

    if let Some(exact) = &spec.json_exact {
        let diffs = matchers::json_exact(exact, &resp.body_json, &spec.ignore, ctx);
        if diffs.is_empty() {
            out.push_pass(format!("{step}: json_exact"));
        } else {
            out.checks.push(with_diffs(
                format!("{step}: response body differs from expected JSON"),
                at("json_exact"),
                exact.clone(),
                diffs,
                &resp.body_json,
            ));
        }
    }

    for (i, assert) in spec.jsonpath.iter().enumerate() {
        let located = JsonPath::parse(&assert.path)
            .ok()
            .map(|p| p.query(&resp.body_json).all())
            .unwrap_or_default();
        let actual = located.first().copied();
        let ops: Value = assert
            .ops
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<serde_json::Map<_, _>>()
            .into();
        if matchers::matches_value(&ops, actual, ctx) {
            out.push_pass(format!("{step}: jsonpath {}", assert.path));
        } else {
            out.checks.push(fail(
                format!("{step}: jsonpath `{}` mismatch", assert.path),
                at(&format!("jsonpath[{i}]")),
                json!({"path": assert.path, "ops": ops}),
                vec![diff(
                    &assert.path,
                    ops,
                    actual.cloned().unwrap_or(Value::Null),
                )],
            ));
        }
    }

    if let Some(body_spec) = &spec.body {
        let ok = match body_spec {
            Value::String(s) => &resp.body == s,
            Value::Object(m) => m
                .get("regex")
                .and_then(Value::as_str)
                .and_then(|p| regex::Regex::new(p).ok())
                .map(|re| re.is_match(&resp.body))
                .unwrap_or(false),
            _ => false,
        };
        if ok {
            out.push_pass(format!("{step}: body"));
        } else {
            out.checks.push(fail(
                format!("{step}: raw body mismatch"),
                at("body"),
                body_spec.clone(),
                vec![diff(
                    "body",
                    body_spec.clone(),
                    json!(truncate(&resp.body, 500)),
                )],
            ));
        }
    }

    if let Some(t) = &spec.time {
        if resp.elapsed <= t.under {
            out.push_pass(format!("{step}: time {}ms", resp.elapsed.as_millis()));
        } else {
            out.checks.push(fail(
                format!(
                    "{step}: took {}ms — expected under {}ms",
                    resp.elapsed.as_millis(),
                    t.under.as_millis()
                ),
                at("time"),
                json!(format!("under {:?}", t.under)),
                vec![],
            ));
        }
    }

    out
}

fn status_ok(spec: &Value, actual: u16) -> bool {
    match spec {
        Value::Number(n) => n.as_u64() == Some(actual as u64),
        Value::String(s) => {
            let s = s.trim();
            if let Some(class) = s.strip_suffix("xx") {
                class
                    .parse::<u16>()
                    .map(|c| actual / 100 == c)
                    .unwrap_or(false)
            } else {
                s.parse::<u16>().map(|v| v == actual).unwrap_or(false)
            }
        }
        Value::Array(options) => options.iter().any(|o| status_ok(o, actual)),
        _ => false,
    }
}

fn fail(
    description: String,
    yaml_path: String,
    expected: Value,
    diffs: Vec<vault_store::FieldDiff>,
) -> CheckResult {
    CheckResult::Fail(CheckFailure {
        description,
        yaml_path,
        expected,
        kind: FailureKind::ValueMismatch { diffs },
        near_misses: vec![],
        attempts: 1,
        elapsed_ms: 0,
    })
}

fn with_diffs(
    description: String,
    yaml_path: String,
    expected: Value,
    diffs: Vec<vault_store::FieldDiff>,
    actual_body: &Value,
) -> CheckResult {
    CheckResult::Fail(CheckFailure {
        description,
        yaml_path,
        expected,
        kind: FailureKind::ValueMismatch { diffs },
        near_misses: vec![vault_store::NearMiss {
            actual: actual_body.clone(),
            diffs: vec![],
            score: 0.0,
        }],
        attempts: 1,
        elapsed_ms: 0,
    })
}

fn diff(path: &str, expected: Value, actual: Value) -> vault_store::FieldDiff {
    vault_store::FieldDiff {
        path: path.to_string(),
        expected,
        actual,
    }
}

fn compact(v: &Value) -> String {
    v.to_string()
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..s.floor_char_boundary(n)])
    }
}
