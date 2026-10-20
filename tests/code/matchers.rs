use serde_json::{json, Value};
use vault_store::matchers::*;

fn ctx() -> MatchCtx {
    MatchCtx {
        anchor_unix_ms: 1_700_000_000_000,
    }
}

#[test]
fn literal_and_operator_matching() {
    assert!(matches_value(&json!(5), Some(&json!(5)), &ctx()));
    assert!(matches_value(&json!(5), Some(&json!(5.0)), &ctx()));
    assert!(!matches_value(&json!(5), Some(&json!(6)), &ctx()));
    assert!(matches_value(
        &json!({"gt": 3, "lt": 10}),
        Some(&json!(5)),
        &ctx()
    ));
    assert!(!matches_value(
        &json!({"gt": 3, "lt": 4}),
        Some(&json!(5)),
        &ctx()
    ));
    assert!(matches_value(
        &json!({"regex": "^ch_"}),
        Some(&json!("ch_123")),
        &ctx()
    ));
    assert!(matches_value(
        &json!({"one_of": ["a", "b"]}),
        Some(&json!("b")),
        &ctx()
    ));
    assert!(matches_value(&json!({"absent": true}), None, &ctx()));
    assert!(!matches_value(
        &json!({"absent": true}),
        Some(&json!(1)),
        &ctx()
    ));
}

#[test]
fn tag_matching() {
    assert!(matches_value(
        &json!({"$tag": "any"}),
        Some(&json!("x")),
        &ctx()
    ));
    assert!(matches_value(
        &json!({"$tag": "uuid"}),
        Some(&json!("6ba7b810-9dad-11d1-80b4-00c04fd430c8")),
        &ctx()
    ));
    assert!(!matches_value(
        &json!({"$tag": "uuid"}),
        Some(&json!("nope")),
        &ctx()
    ));
    assert!(matches_value(
        &json!({"$tag": "not-null"}),
        Some(&json!(0)),
        &ctx()
    ));
    assert!(matches_value(
        &json!({"$tag": "null"}),
        Some(&Value::Null),
        &ctx()
    ));
}

#[test]
fn near_now_within_tolerance() {
    let c = ctx();
    let anchor = chrono::DateTime::from_timestamp_millis(c.anchor_unix_ms).unwrap();
    let ts = (anchor + chrono::Duration::seconds(2)).to_rfc3339();
    assert!(matches_value(
        &json!({"$tag": "near-now", "arg": "5s"}),
        Some(&json!(ts)),
        &c
    ));
    let far = (anchor + chrono::Duration::seconds(60)).to_rfc3339();
    assert!(!matches_value(
        &json!({"$tag": "near-now", "arg": "5s"}),
        Some(&json!(far)),
        &c
    ));
}

#[test]
fn containment_reports_diffs() {
    let exp = json!({"status": "pending", "user": {"id": 1}});
    let act = json!({"status": "confirmed", "user": {"id": 1}, "extra": true});
    let diffs = json_contains(&exp, &act, &ctx());
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path, "$.status");
}

#[test]
fn containment_arrays_unordered_subset() {
    let exp = json!([{"sku": "B"}, {"sku": "A"}]);
    let act = json!([{"sku": "A", "qty": 1}, {"sku": "B", "qty": 2}]);
    assert!(json_contains(&exp, &act, &ctx()).is_empty());
}

#[test]
fn exact_flags_extra_keys_and_respects_ignore() {
    let exp = json!({"a": 1});
    let act = json!({"a": 1, "b": 2});
    let diffs = json_exact(&exp, &act, &[], &ctx());
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path, "$.b");
    assert!(json_exact(&exp, &act, &["$.b".into()], &ctx()).is_empty());
}

#[test]
fn bipartite_avoids_greedy_false_negative() {
    // exp0 matches only call 0; exp1 matches calls 0 and 1: greedy order
    // could starve exp0, exact matching must not.
    let adj = vec![vec![0], vec![0, 1]];
    let m = max_bipartite(2, &adj);
    assert_eq!(m[0], Some(0));
    assert_eq!(m[1], Some(1));
}

#[test]
fn score_ranks_near_misses() {
    let exp = json!({"user_id": 1, "status": "confirmed", "total": 4200});
    let close = json!({"user_id": 1, "status": "pending", "total": 4200});
    let far = json!({"user_id": 9, "status": "x", "total": 1});
    let (s1, d1) = score_object(&exp, &close, &ctx());
    let (s2, _) = score_object(&exp, &far, &ctx());
    assert!(s1 > s2);
    assert_eq!(d1.len(), 1);
    assert_eq!(d1[0].path, "status");
}

#[test]
fn regex_never_matches_sql_null() {
    assert!(!matches_value(
        &json!({"regex": ".+"}),
        Some(&Value::Null),
        &ctx()
    ));
}

#[test]
fn big_integers_compare_exactly() {
    let a = json!(1_700_000_000_000_000_001_i64);
    let b = json!(1_700_000_000_000_000_002_i64);
    assert!(!matches_value(&a, Some(&b), &ctx()));
    assert!(matches_value(&a, Some(&a), &ctx()));
    assert!(matches_value(&json!(5), Some(&json!(5.0)), &ctx()));
}

#[test]
fn durations_accept_compound_forms_and_days() {
    assert_eq!(humantime_ms("1m30s"), Some(90_000));
    assert_eq!(humantime_ms("1d"), Some(86_400_000));
    assert_eq!(humantime_ms("500ms"), Some(500));
    assert_eq!(humantime_ms("zzz"), None);
    // 12 hours before the ctx() anchor (2023-11-14T22:13:20Z).
    let stale = json!("2023-11-14T10:13:20Z");
    assert!(matches_value(
        &json!({"$tag": "near-now", "arg": "1d"}),
        Some(&stale),
        &ctx()
    ));
    assert!(!matches_value(
        &json!({"$tag": "near-now", "arg": "5s"}),
        Some(&stale),
        &ctx()
    ));
    let fresh = json!("2023-11-14T22:13:20Z");
    assert!(!matches_value(
        &json!({"$tag": "near-now", "arg": "zzz"}),
        Some(&fresh),
        &ctx()
    ));
}

#[test]
fn check_expected_rejects_matchers_that_could_never_match() {
    assert!(check_expected(&json!({"regex": "("})).is_err());
    assert!(check_expected(&json!({"regex": "^{{ x }}$"})).is_ok());
    assert!(check_expected(&json!({"$tag": "not_null"})).is_err());
    assert!(check_expected(&json!({"$tag": "near-now", "arg": "1m30s"})).is_ok());
    assert!(check_expected(&json!({"$tag": "near-now", "arg": "soon"})).is_err());
    assert!(check_expected(&json!({"id": 1, "status": {"regex": "(oops"}})).is_err());
    assert!(check_expected(&json!({"id": 1, "status": {"one_of": ["a", "b"]}})).is_ok());
}

#[test]
fn unsigned_integers_numeric_tolerances_and_shape_checks() {
    let big = json!(18_446_744_073_709_551_615_u64);
    assert!(matches_value(&big, Some(&big), &ctx()));
    assert!(!matches_value(
        &big,
        Some(&json!(18_446_744_073_709_551_614_u64)),
        &ctx()
    ));
    // A numeric near-now argument is seconds; this timestamp is 60s before the anchor.
    let stale = json!("2023-11-14T22:12:20Z");
    assert!(matches_value(
        &json!({"$tag": "near-now", "arg": 120}),
        Some(&stale),
        &ctx()
    ));
    assert!(!matches_value(
        &json!({"$tag": "near-now", "arg": 30}),
        Some(&stale),
        &ctx()
    ));
    assert!(check_expected(&json!({"$tag": "one-of", "arg": 5})).is_err());
    assert!(check_expected(&json!({"one_of": "archived"})).is_err());
    assert!(check_expected(&json!({"gt": "abc"})).is_err());
    assert!(check_expected(&json!({"gt": "{{ vars.min }}"})).is_ok());
    assert!(check_expected(&json!({"$tag": "json"})).is_err());
    assert!(check_duration(&json!("1m30s")).is_ok());
    assert!(check_duration(&json!(5)).is_ok());
    assert!(check_duration(&json!("soon")).is_err());
}
