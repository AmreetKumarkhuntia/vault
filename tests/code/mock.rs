use indexmap::IndexMap;
use serde_json::{json, Value};
use vault_dsl::{CallExpect, MatchSpec, UnmatchedPolicy};
use vault_mock::matcher::{component_score, full_score, path_matches, request_matches};
use vault_mock::record::{RecordedExchange, RecordedRequest};
use vault_mock::session::SessionReport;
use vault_mock::verify_calls;
use vault_store::matchers::MatchCtx;
use vault_store::FailureKind;

fn req(method: &str, path: &str, body: Value) -> RecordedRequest {
    RecordedRequest {
        method: method.into(),
        path: path.into(),
        query: json!({}),
        headers: json!({}),
        body: body.to_string(),
        body_json: body,
    }
}

#[test]
fn path_params_are_captured() {
    let p = path_matches(&json!("/v1/charges/{id}"), "/v1/charges/ch_42").unwrap();
    assert_eq!(p["id"], "ch_42");
    assert!(path_matches(&json!("/v1/charges/{id}"), "/v1/refunds/x").is_none());
    assert!(path_matches(&json!("/v1/**"), "/v1/a/b/c").is_some());
}

#[test]
fn full_match_requires_every_component() {
    let spec: MatchSpec = serde_json::from_value(json!({
        "method": "POST",
        "path": "/v1/charges",
        "body": { "json_partial": { "currency": "USD" } }
    }))
    .unwrap();
    assert!(request_matches(
        &spec,
        &req("POST", "/v1/charges", json!({"currency":"USD","x":1}))
    ));
    assert!(!request_matches(
        &spec,
        &req("POST", "/v1/charges", json!({"currency":"EUR"}))
    ));
    assert!(!request_matches(
        &spec,
        &req("GET", "/v1/charges", json!({"currency":"USD"}))
    ));
}

#[test]
fn near_miss_score_is_partial() {
    let spec: MatchSpec = serde_json::from_value(json!({
        "method": "POST",
        "path": "/v1/charges",
        "body": { "json_partial": { "amount": 100 } }
    }))
    .unwrap();
    let s = component_score(&spec, &req("POST", "/v1/charges", json!({"amount": 99})));
    assert_eq!(s, 5);
    assert_eq!(full_score(&spec), 7);
}

fn rec(seq: u64, dep: &str, method: &str, path: &str, body: Value) -> RecordedExchange {
    RecordedExchange {
        seq,
        at_unix_ms: 0,
        dependency: dep.into(),
        request: req(method, path, body),
        matched_stub: Some("s".into()),
        responded_status: 200,
        render_error: None,
    }
}

fn report(recordings: Vec<RecordedExchange>, deps: &[&str]) -> SessionReport {
    SessionReport {
        recordings,
        mocked_deps: deps.iter().map(|s| s.to_string()).collect(),
        unmatched_hits: vec![],
        base_urls: IndexMap::new(),
    }
}

fn expect(name: &str, dep: &str, method: &str, path: &str, count: Value) -> CallExpect {
    serde_json::from_value(json!({
        "name": name,
        "dependency": dep,
        "match": { "method": method, "path": path },
        "count": count,
    }))
    .unwrap()
}

#[test]
fn missed_call_reports_near_miss() {
    let r = report(
        vec![rec(0, "payments", "POST", "/v1/refunds", json!({}))],
        &["payments"],
    );
    let calls = vec![expect(
        "charge",
        "payments",
        "POST",
        "/v1/charges",
        json!(1),
    )];
    let out = verify_calls(
        &r,
        &calls,
        &[],
        &UnmatchedPolicy::Allow,
        &MatchCtx::default(),
    );
    let fails: Vec<_> = out.failures().collect();
    assert_eq!(fails.len(), 1);
    assert!(matches!(
        fails[0].kind,
        FailureKind::MissedCall { satisfied: 0 }
    ));
    assert!(!fails[0].near_misses.is_empty());
}

#[test]
fn overlapping_expectations_use_bipartite_not_greedy() {
    let r = report(
        vec![
            rec(0, "email", "POST", "/send", json!({"to": "a@x"})),
            rec(1, "email", "POST", "/send", json!({"to": "b@x"})),
        ],
        &["email"],
    );
    let mut specific = expect("specific", "email", "POST", "/send", json!(1));
    specific.body = Some(json!({"json_partial": {"to": "a@x"}}));
    let broad = expect("broad", "email", "POST", "/send", json!(1));
    // Broad listed first: greedy would give it recording 0 and starve specific.
    let out = verify_calls(
        &r,
        &[broad, specific],
        &[],
        &UnmatchedPolicy::Allow,
        &MatchCtx::default(),
    );
    assert!(out.passed(), "{:?}", out.failures().collect::<Vec<_>>());
}

#[test]
fn count_zero_means_never_called() {
    let r = report(
        vec![rec(0, "email", "POST", "/send", json!({}))],
        &["email"],
    );
    let calls = vec![expect("no emails", "email", "POST", "/send", json!(0))];
    let out = verify_calls(
        &r,
        &calls,
        &[],
        &UnmatchedPolicy::Allow,
        &MatchCtx::default(),
    );
    let fails: Vec<_> = out.failures().collect();
    assert_eq!(fails.len(), 1);
    assert!(matches!(fails[0].kind, FailureKind::CountMismatch { .. }));
}

#[test]
fn unexpected_calls_fail_when_policy_is_fail() {
    let r = report(
        vec![rec(0, "email", "POST", "/send", json!({}))],
        &["email"],
    );
    let out = verify_calls(&r, &[], &[], &UnmatchedPolicy::Fail, &MatchCtx::default());
    let fails: Vec<_> = out.failures().collect();
    assert_eq!(fails.len(), 1);
    assert!(matches!(fails[0].kind, FailureKind::UnexpectedCall { .. }));
}

#[test]
fn ordered_group_checks_subsequence() {
    let r = report(
        vec![
            rec(0, "email", "POST", "/send", json!({})),
            rec(1, "payments", "POST", "/v1/charges", json!({})),
        ],
        &["email", "payments"],
    );
    let calls = vec![
        expect("charge", "payments", "POST", "/v1/charges", json!(1)),
        expect("mail", "email", "POST", "/send", json!(1)),
    ];
    let bad = vec![vec!["charge".to_string(), "mail".to_string()]];
    let out = verify_calls(
        &r,
        &calls,
        &bad,
        &UnmatchedPolicy::Allow,
        &MatchCtx::default(),
    );
    assert!(out
        .failures()
        .any(|f| matches!(f.kind, FailureKind::OrderViolation { .. })));

    let good = vec![vec!["mail".to_string(), "charge".to_string()]];
    let out = verify_calls(
        &r,
        &calls,
        &good,
        &UnmatchedPolicy::Allow,
        &MatchCtx::default(),
    );
    assert!(out.passed());
}

#[test]
fn count_zero_sees_recordings_absorbed_by_a_broader_expectation() {
    let r = report(
        vec![rec(
            0,
            "email",
            "POST",
            "/send",
            json!({"template": "welcome"}),
        )],
        &["email"],
    );
    let some_email = expect("some email", "email", "POST", "/send", json!({"gte": 1}));
    let mut no_welcome = expect("no welcome", "email", "POST", "/send", json!(0));
    no_welcome.body = Some(json!({"json_partial": {"template": "welcome"}}));
    let out = verify_calls(
        &r,
        &[some_email, no_welcome],
        &[],
        &UnmatchedPolicy::Allow,
        &MatchCtx::default(),
    );
    let fails: Vec<_> = out.failures().collect();
    assert_eq!(fails.len(), 1, "{fails:?}");
    assert!(matches!(fails[0].kind, FailureKind::CountMismatch { .. }));
}

#[test]
fn bounded_optional_expectations_do_not_steal_from_exact_ones() {
    let r = report(
        vec![
            rec(0, "email", "POST", "/send", json!({"template": "welcome"})),
            rec(1, "email", "POST", "/send", json!({"template": "receipt"})),
        ],
        &["email"],
    );
    let one_mail = expect("one mail", "email", "POST", "/send", json!(1));
    let mut welcome = expect(
        "welcome optional",
        "email",
        "POST",
        "/send",
        json!({"lte": 1}),
    );
    welcome.body = Some(json!({"json_partial": {"template": "welcome"}}));
    let out = verify_calls(
        &r,
        &[one_mail, welcome],
        &[],
        &UnmatchedPolicy::Allow,
        &MatchCtx::default(),
    );
    assert!(out.passed(), "{:?}", out.failures().collect::<Vec<_>>());
}

#[test]
fn header_and_query_matchers_share_one_rule() {
    let spec: MatchSpec = serde_json::from_value(json!({
        "headers": { "x-count": 3, "x-debug": { "$tag": "absent" } },
        "query": { "page": { "gte": 1 }, "id": "1" }
    }))
    .unwrap();
    let mut request = req("GET", "/x", json!({}));
    request.headers = json!({ "x-count": "3" });
    request.query = json!({ "page": "2", "id": ["2", "1"] });
    assert!(request_matches(&spec, &request));
    request.headers = json!({ "x-count": "3", "x-debug": "1" });
    assert!(!request_matches(&spec, &request));
}

#[test]
fn longest_prefix_wins_and_prefix_is_normalised() {
    let dep = |prefix: &str| -> vault_dsl::MockDep {
        serde_json::from_value(json!({ "prefix": prefix, "stubs": [] })).unwrap()
    };
    let mut mocks = IndexMap::new();
    mocks.insert("api".to_string(), dep("/api"));
    mocks.insert("billing".to_string(), dep("/api/billing/"));
    mocks.insert("payments".to_string(), dep("payments"));
    let render: vault_mock::ResponseRenderer = std::sync::Arc::new(|r: &Value, _| Ok(r.clone()));
    let session = vault_mock::Session::new("t", &mocks, UnmatchedPolicy::Allow, render);
    let (dep, rest) = vault_mock::session::resolve_dep(&session, "/api/billing/v1/x").unwrap();
    assert_eq!((dep.name.as_str(), rest.as_str()), ("billing", "/v1/x"));
    let (dep, rest) = vault_mock::session::resolve_dep(&session, "/payments/v1/charges").unwrap();
    assert_eq!(
        (dep.name.as_str(), rest.as_str()),
        ("payments", "/v1/charges")
    );
    assert!(vault_mock::session::resolve_dep(&session, "/apix/v1").is_none());
}

#[test]
fn contradictory_count_bounds_fail_the_check_instead_of_overflowing() {
    let r = report(
        vec![
            rec(0, "email", "POST", "/send", json!({})),
            rec(1, "email", "POST", "/send", json!({})),
        ],
        &["email"],
    );
    let calls = vec![expect(
        "impossible",
        "email",
        "POST",
        "/send",
        json!({"gte": 3, "lte": 1}),
    )];
    let out = verify_calls(
        &r,
        &calls,
        &[],
        &UnmatchedPolicy::Allow,
        &MatchCtx::default(),
    );
    assert!(!out.passed());
}

#[test]
fn repeated_query_keys_match_operators_on_any_element() {
    let spec: MatchSpec =
        serde_json::from_value(json!({"query": {"tag": {"regex": "^a$"}}})).unwrap();
    let mut request = req("GET", "/x", json!({}));
    request.query = json!({"tag": ["b", "a"]});
    assert!(request_matches(&spec, &request));
    request.query = json!({"tag": ["b", "c"]});
    assert!(!request_matches(&spec, &request));
}
