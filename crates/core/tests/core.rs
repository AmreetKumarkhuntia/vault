use serde_json::{json, Value};
use std::time::Duration;
use vault_core::{assert_response, StepResponse, TemplateEngine};
use vault_dsl::ExpectSpec;
use vault_store::matchers::MatchCtx;
use vault_store::FailureKind;

fn engine() -> TemplateEngine {
    TemplateEngine::new(
        json!({
            "vars": {"user_id": 7},
            "steps": {"create": {"captures": {"order_id": 42}}},
            "order_id": 42,
            "mock": {"payments": {"base_url": "http://127.0.0.1:9/payments"}},
        }),
        1_700_000_000_000,
    )
}

#[test]
fn single_expression_keeps_native_type() {
    let e = engine();
    assert_eq!(e.render_value(&json!("{{ order_id }}")).unwrap(), json!(42));
    assert_eq!(
        e.render_value(&json!("{{ steps.create.captures.order_id }}")).unwrap(),
        json!(42)
    );
    assert_eq!(e.render_value(&json!("{{ vars.user_id }}")).unwrap(), json!(7));
}

#[test]
fn mixed_strings_render_to_text() {
    let e = engine();
    assert_eq!(
        e.render_value(&json!("/api/orders/{{ order_id }}")).unwrap(),
        json!("/api/orders/42")
    );
}

#[test]
fn renders_nested_docs() {
    let e = engine();
    let doc = json!({"path": "/x/{{ order_id }}", "body": {"id": "{{ order_id }}"}});
    let out = e.render_value(&doc).unwrap();
    assert_eq!(out["path"], "/x/42");
    assert_eq!(out["body"]["id"], 42);
}

#[test]
fn now_is_frozen() {
    let e = engine();
    let a = e.render_value(&json!("{{ now() }}")).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let b = e.render_value(&json!("{{ now() }}")).unwrap();
    assert_eq!(a, b);
}

#[test]
fn unknown_reference_errors() {
    let e = engine();
    assert!(e.render_value(&json!("{{ steps.nope.captures.x }}")).is_err());
}

fn resp(status: u16, body: Value) -> StepResponse {
    StepResponse {
        status,
        headers: json!({"content-type": "application/json"}),
        body: body.to_string(),
        body_json: body,
        elapsed: Duration::from_millis(10),
    }
}

fn spec(v: Value) -> ExpectSpec {
    serde_json::from_value(v).unwrap()
}

#[test]
fn status_classes_and_lists() {
    let ctx = MatchCtx::default();
    assert!(assert_response("s", &spec(json!({"status": "2xx"})), &resp(204, json!(null)), &ctx)
        .passed());
    assert!(assert_response(
        "s",
        &spec(json!({"status": [200, 201]})),
        &resp(201, json!(null)),
        &ctx
    )
    .passed());
    assert!(!assert_response("s", &spec(json!({"status": 200})), &resp(404, json!(null)), &ctx)
        .passed());
}

#[test]
fn jsonpath_ops() {
    let ctx = MatchCtx::default();
    let r = resp(200, json!({"total": 42, "items": [1, 2]}));
    let s = spec(json!({"jsonpath": [
        {"path": "$.total", "gt": 0},
        {"path": "$.items", "len": 2},
        {"path": "$.missing", "absent": true}
    ]}));
    let out = assert_response("s", &s, &r, &ctx);
    assert!(out.passed(), "{:?}", out.failures().collect::<Vec<_>>());
}

#[test]
fn json_partial_failure_carries_diffs() {
    let ctx = MatchCtx::default();
    let r = resp(200, json!({"status": "confirmed"}));
    let out = assert_response("s", &spec(json!({"json_partial": {"status": "pending"}})), &r, &ctx);
    let fails: Vec<_> = out.failures().collect();
    assert_eq!(fails.len(), 1);
    match &fails[0].kind {
        FailureKind::ValueMismatch { diffs } => assert_eq!(diffs[0].path, "$.status"),
        other => panic!("wrong kind: {other:?}"),
    }
}
