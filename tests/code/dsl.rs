use serde_yaml_ng::Value as Yaml;
use vault_dsl::*;

#[test]
fn tags_become_matcher_objects() {
    let y: Yaml = serde_yaml_ng::from_str("id: !uuid\ncreated: !near-now 5s\n").unwrap();
    let j = yaml_to_json(&y);
    assert_eq!(j["id"]["$tag"], "uuid");
    assert_eq!(j["created"]["$tag"], "near-now");
    assert_eq!(j["created"]["arg"], "5s");
}

#[test]
fn env_interpolation_with_default() {
    std::env::remove_var("TK_MISSING_XYZ");
    assert_eq!(
        interpolate_env("url: ${env.TK_MISSING_XYZ:-postgres://localhost/x}"),
        "url: postgres://localhost/x"
    );
    std::env::set_var("TK_SET_XYZ", "hello");
    assert_eq!(interpolate_env("${env.TK_SET_XYZ}"), "hello");
}

#[test]
fn parses_a_full_test_definition() {
    let raw = r#"
test: create order
tags: [orders]
timeout: 30s
seed:
  postgres:
    - table: users
      rows: [{ id: 1, email: a@b.c }]
  redis:
    - set: { key: "session:a", value: tok, ttl: 60 }
mocks:
  payments:
    unmatched: fail
    stubs:
      - name: charge-ok
        match: { method: POST, path: /v1/charges }
        response: { status: 201, json: { id: "ch_1" }, latency: 30ms }
        times: 2
steps:
  - name: create
    request: { method: POST, path: /api/orders, json: { user_id: 1 } }
    expect:
      status: 201
      json_partial: { status: pending }
      jsonpath:
        - { path: $.total, gt: 0 }
    capture:
      order_id: { jsonpath: $.id }
    repeat: { every: 200ms, timeout: 5s }
verify:
  postgres:
    - table: orders
      expect:
        - user_id: 1
          id: !uuid
          created_at: !near-now 5s
      count: exact
      eventually: 5s
  calls:
    - name: one charge
      dependency: payments
      match: { method: POST, path: /v1/charges }
      count: 1
  ordered: [[one charge]]
  unexpected: fail
"#;
    let def: TestDef = parse_str(raw, "inline").unwrap();
    assert_eq!(def.test, "create order");
    assert_eq!(def.steps.len(), 1);
    assert_eq!(def.steps[0].capture.len(), 1);
    assert_eq!(def.mocks["payments"].stubs[0].times, Some(2));
    assert_eq!(def.verify.calls.len(), 1);
    let pg = &def.verify.stores["postgres"];
    assert_eq!(pg[0]["expect"][0]["id"]["$tag"], "uuid");
    assert_eq!(pg[0]["eventually"], "5s");
}

#[test]
fn parses_a_flow_definition() {
    let raw = r#"
flow: order lifecycle
reset: once
stages:
  - test: create order
    export: [order_id]
  - test: update order
    with: { order_id: "{{ flow.order_id }}" }
on_failure: skip-rest
"#;
    let def: FlowDef = parse_str(raw, "inline").unwrap();
    assert_eq!(def.stages.len(), 2);
    assert_eq!(def.stages[0].export, vec!["order_id"]);
    assert_eq!(def.reset, FlowReset::Once);
}

#[test]
fn unknown_fields_are_rejected() {
    let raw = "test: x\nstepz: []\n";
    let err = parse_str::<TestDef>(raw, "inline").unwrap_err();
    assert!(err.to_string().contains("stepz"), "got: {err}");
}

fn suite_with(tests: Vec<TestDef>, flows: Vec<FlowDef>) -> Suite {
    Suite {
        root: "tests".into(),
        config: parse_str("environments: { local: { target: { base_url: http://x } } }", "cfg")
            .unwrap(),
        tests: tests
            .into_iter()
            .map(|def| LoadedTest { path: "t.test.yaml".into(), def })
            .collect(),
        flows: flows
            .into_iter()
            .map(|def| LoadedFlow { path: "f.flow.yaml".into(), def })
            .collect(),
    }
}

#[test]
fn catches_undeclared_call_dependency_and_dup_steps() {
    let t: TestDef = parse_str(
        r#"
test: bad
steps:
  - name: a
    request: { method: GET, path: /x }
  - name: a
    request: { method: GET }
verify:
  calls:
    - dependency: payments
      match: { method: POST, path: /v1/charges }
"#,
        "inline",
    )
    .unwrap();
    let issues = validate_suite(&suite_with(vec![t], vec![]));
    let text = issues.iter().map(|i| i.message.clone()).collect::<Vec<_>>().join("\n");
    assert!(text.contains("duplicate step name"), "{text}");
    assert!(text.contains("needs `path` or `url`"), "{text}");
    assert!(text.contains("not declared under mocks"), "{text}");
}

#[test]
fn catches_unexported_flow_reference() {
    let t1: TestDef = parse_str(
        "test: a\nsteps: [{name: s, request: {method: GET, path: /x}, capture: {order_id: {jsonpath: $.id}}}]",
        "inline",
    )
    .unwrap();
    let t2: TestDef =
        parse_str("test: b\nsteps: [{name: s, request: {method: GET, path: /y}}]", "inline")
            .unwrap();
    let flow: FlowDef = parse_str(
        r#"
flow: f
stages:
  - test: a
  - test: b
    with: { order_id: "{{ flow.order_id }}" }
"#,
        "inline",
    )
    .unwrap();
    let issues = validate_suite(&suite_with(vec![t1, t2], vec![flow]));
    assert!(
        issues.iter().any(|i| i.message.contains("not exported by any earlier stage")),
        "{issues:?}"
    );
}

#[test]
fn env_interpolation_nested_default() {
    std::env::remove_var("TK_OUTER_ZZZ");
    std::env::set_var("TK_INNER_ZZZ", "alice");
    assert_eq!(
        interpolate_env("${env.TK_OUTER_ZZZ:-postgres://${env.TK_INNER_ZZZ}@localhost/db}"),
        "postgres://alice@localhost/db"
    );
}
