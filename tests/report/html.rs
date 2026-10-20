use serde_json::{json, Value};
use vault_report::{to_html, write_html, HtmlReport};

fn report() -> HtmlReport {
    HtmlReport {
        schema_version: 2,
        run: json!({
            "schema_version": 2,
            "environment": "local",
            "duration_ms": 71,
            "tests": [
                {"name":"same name","status":"PASSED","duration_ms":11,"steps":[],"verify":{"checks":[]}},
                {"name":"same name","status":"FAILED","duration_ms":12,"steps":[],"verify":{"checks":[]}},
                {"name":"error","status":"ERRORED","error":"connection failed","steps":[]},
                {"name":"skip","status":"SKIPPED","skip_reason":"dependency failed","steps":[]}
            ]
        }),
        metadata: json!({
            "started_at":"2026-10-03T12:00:00Z",
            "items":[{"id":"flow-0","name":"Order flow","kind":"flow","reset":"once","on_failure":"skip-rest"}],
            "tests":[
                {"id":"first","item_id":"flow-0","name":"same name","flow":"Order flow","stage_index":0,"result_index":0,"execution_index":0},
                {"id":"second","item_id":"flow-0","name":"same name","flow":"Order flow","stage_index":1,"result_index":1,"execution_index":1},
                {"id":"third","name":"error","result_index":2},
                {"id":"fourth","name":"skip","result_index":3},
                {"id":"fifth","name":"not reached","result_index":null}
            ]
        }),
        executions: vec![
            json!({"events":[{"phase":"step","subject":"create","status":"PASSED","details":{"request":{"method":"POST","path":"/orders"}}}]}),
            json!({"events":[{"phase":"verify","status":"FAILED","details":{"checks":{"checks":[{"result":"fail","description":"wrong status","kind":{"kind":"value_mismatch","diffs":[{"path":"status","expected":"ready","actual":"pending"}]}}]}}}]}),
        ],
    }
}

fn embedded_data(html: &str) -> Value {
    let start = html
        .find("<script id=\"vault-report-data\" type=\"application/json\">")
        .unwrap()
        + "<script id=\"vault-report-data\" type=\"application/json\">".len();
    let end = html[start..].find("</script>").unwrap() + start;
    serde_json::from_str(&html[start..end]).unwrap()
}

#[test]
fn embeds_statuses_order_and_repeated_test_identity() {
    let html = to_html(&report());
    let data = embedded_data(&html);
    assert_eq!(data["schema_version"], 2);
    assert_eq!(data["metadata"]["tests"][0]["id"], "first");
    assert_eq!(data["metadata"]["tests"][1]["id"], "second");
    assert_eq!(data["metadata"]["tests"][4]["result_index"], Value::Null);
    for status in ["PASSED", "FAILED", "ERRORED", "SKIPPED", "NOT_RUN"] {
        assert!(html.contains(status));
    }
    assert!(html.contains("Summary above shows whole-run totals"));
    assert!(html.contains("Execution lifecycle"));
    assert!(!html.contains("__VAULT_"));
    assert!(!html.contains("fetch("));
    assert!(!html.contains("https://"));
    assert!(html.contains("<style>"));
    assert!(html.contains(":root{color-scheme:"));
    assert!(!html.contains("rel=\"stylesheet\""));
}

#[test]
fn safely_embeds_hostile_markup_and_unicode_without_changing_evidence() {
    let mut input = report();
    let hostile = "</script><img src=x onerror=alert(1)> & 雪 🦀 \u{2028}\u{2029}";
    input.run["tests"][0]["name"] = json!(hostile);
    input.executions[0]["events"][0]["details"]["response"] = json!({"body": hostile});
    let html = to_html(&input);
    assert!(!html.contains("</script><img"));
    assert!(html.contains("\\u003c/script\\u003e"));
    assert!(html.contains("\\u2028\\u2029"));
    let data = embedded_data(&html);
    assert_eq!(data["run"]["tests"][0]["name"], hostile);
    assert_eq!(
        data["executions"][0]["events"][0]["details"]["response"]["body"],
        hostile
    );
    assert!(!html.contains(".innerHTML"));
}

#[test]
fn truncates_embedded_payloads_on_utf8_boundary_and_preserves_action_metadata() {
    let mut input = report();
    let body = format!("{}END_SECRET_SENTINEL", "🦀".repeat(20_000));
    input.executions[0]["events"][0]["details"]["response"] = json!({"status":200,"body":body});
    let html = to_html(&input);
    let data = embedded_data(&html);
    let event = &data["executions"][0]["events"][0];
    let preview = &event["details"]["response"]["body"];
    assert_eq!(preview["_vault_preview"], true);
    assert_eq!(preview["preview"].as_str().unwrap().len(), 64 * 1024);
    assert_eq!(preview["original_bytes"], body.len());
    assert!(preview["omitted_bytes"].as_u64().unwrap() > 0);
    assert_eq!(event["subject"], "create");
    assert_eq!(event["details"]["request"]["path"], "/orders");
    assert_eq!(data["metadata"]["tests"].as_array().unwrap().len(), 5);
    assert!(!html.contains("END_SECRET_SENTINEL"));
}

#[test]
fn large_recordings_keep_each_call_and_bound_its_payload() {
    let mut input = report();
    input.run["tests"][0]["recorded_calls"] = json!([
        {"seq":1,"dependency":"payments","request":{"path":"/first","body":"a".repeat(80_000)}},
        {"seq":2,"dependency":"payments","request":{"path":"/second","body":"b".repeat(80_000)}}
    ]);
    let data = embedded_data(&to_html(&input));
    let calls = data["run"]["tests"][0]["recorded_calls"]
        .as_array()
        .unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["request"]["path"], "/first");
    assert_eq!(calls[1]["request"]["path"], "/second");
    assert_eq!(calls[0]["request"]["body"]["original_bytes"], 80_000);
    assert_eq!(calls[1]["request"]["body"]["original_bytes"], 80_000);
}

#[test]
fn large_diff_values_keep_preview_metadata_for_visible_omission_notices() {
    let mut input = report();
    input.run["tests"][1]["verify"]["checks"] = json!([{
        "result":"fail",
        "description":"large response mismatch",
        "kind":{
            "kind":"value_mismatch",
            "diffs":[{
                "path":"body",
                "expected":"x".repeat(80_000),
                "actual":"y".repeat(90_000)
            }]
        }
    }]);
    let html = to_html(&input);
    let data = embedded_data(&html);
    let diff = &data["run"]["tests"][1]["verify"]["checks"][0]["kind"]["diffs"][0];
    assert_eq!(diff["path"], "body");
    for (field, original_bytes) in [("expected", 80_000), ("actual", 90_000)] {
        assert_eq!(diff[field]["_vault_preview"], true);
        assert_eq!(diff[field]["preview"].as_str().unwrap().len(), 65_536);
        assert_eq!(diff[field]["original_bytes"], original_bytes);
        assert_eq!(diff[field]["omitted_bytes"], original_bytes - 65_536);
    }
    assert!(html.contains("Preview truncated at 64 KiB"));
}

#[test]
fn empty_and_unknown_optional_values_remain_valid_reports() {
    let input = HtmlReport {
        schema_version: 2,
        run: json!({"tests":[]}),
        metadata: json!({}),
        executions: vec![],
    };
    let data = embedded_data(&to_html(&input));
    assert_eq!(data["run"]["tests"], json!([]));
    assert_eq!(data["executions"], json!([]));
}

#[test]
fn bounds_large_observed_call_order_and_retains_original_entry_count() {
    let mut input = report();
    let interleaving: Vec<Value> = (0..3_000)
        .map(|index| json!([format!("call-{index}-{}", "雪".repeat(7)), index]))
        .collect();
    let original_bytes = serde_json::to_string_pretty(&interleaving).unwrap().len();
    input.run["tests"][1]["verify"]["checks"] = json!([{
        "result":"fail", "description":"calls happened in the wrong order",
        "kind":{"kind":"order_violation", "interleaving":interleaving}
    }]);
    let data = embedded_data(&to_html(&input));
    let kind = &data["run"]["tests"][1]["verify"]["checks"][0]["kind"];
    assert_eq!(kind["kind"], "order_violation");
    assert_eq!(kind["interleaving"]["_vault_preview"], true);
    assert!(kind["interleaving"]["preview"].as_str().unwrap().len() <= 65_536);
    assert_eq!(kind["interleaving"]["original_entries"], 3_000);
    assert_eq!(kind["interleaving"]["original_bytes"], original_bytes);
    assert!(kind["interleaving"]["omitted_bytes"].as_u64().unwrap() > 0);
}

#[test]
fn bounds_captured_values_and_stage_inputs_without_losing_execution_identity() {
    let mut input = report();
    let captured = format!("{}CAPTURE_TAIL_SENTINEL", "雪".repeat(30_000));
    input.executions[0]["events"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "phase":"capture", "subject":"response_body", "status":"PASSED",
            "step_index":0, "details":{"value":captured}
        }));
    let stage_input = format!("{}STAGE_INPUT_TAIL_SENTINEL", "a".repeat(90_000));
    input.metadata["tests"][0]["with"] = json!({"payload":stage_input});
    let input_bytes = serde_json::to_string_pretty(&input.metadata["tests"][0]["with"])
        .unwrap()
        .len();
    let html = to_html(&input);
    let data = embedded_data(&html);
    let events = data["executions"][0]["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1]["phase"], "capture");
    assert_eq!(events[1]["subject"], "response_body");
    assert_eq!(events[1]["step_index"], 0);
    let value = &events[1]["details"]["value"];
    assert_eq!(value["_vault_preview"], true);
    assert!(value["preview"].as_str().unwrap().len() <= 65_536);
    assert_eq!(value["original_bytes"], captured.len());
    let descriptor = &data["metadata"]["tests"][0];
    assert_eq!(descriptor["id"], "first");
    assert_eq!(descriptor["name"], "same name");
    assert_eq!(descriptor["stage_index"], 0);
    assert_eq!(descriptor["with"]["_vault_preview"], true);
    assert_eq!(descriptor["with"]["original_bytes"], input_bytes);
    assert_eq!(descriptor["with"]["original_entries"], 1);
    assert!(!html.contains("CAPTURE_TAIL_SENTINEL"));
    assert!(!html.contains("STAGE_INPUT_TAIL_SENTINEL"));
}

#[test]
fn writes_a_report_with_external_stylesheet_and_creates_parent_directories() {
    let root = std::env::temp_dir().join(format!(
        "vault-html-write-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let path = root.join("nested/report.html");
    write_html(&report(), path.to_str().unwrap()).unwrap();
    let html = std::fs::read_to_string(&path).unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert_eq!(embedded_data(&html)["schema_version"], 2);
    assert!(html.contains("<link rel=\"stylesheet\" href=\"vault-report-pages/style.css\">"));
    assert!(!html.contains("<style"));
    let css = std::fs::read_to_string(root.join("nested/vault-report-pages/style.css")).unwrap();
    assert!(css.contains(":root{color-scheme:"));
    assert!(css.contains(".report-index-table"));
    std::fs::remove_dir_all(root).unwrap();
}
