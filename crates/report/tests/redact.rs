use serde_json::{json, Value};
use vault_core::{ResponseSummary, RunResult, StepResult, TestResult, TestStatus};
use vault_dsl::ReportRedaction;
use vault_report::{to_junit_xml, Redactor};
use vault_store::{CheckFailure, CheckResult, FailureKind, FieldDiff, NearMiss, VerifyOutcome};

fn generated_fixture_value() -> String {
    static NEXT_VALUE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
    format!(
        "fixture-{}-{}",
        std::process::id(),
        NEXT_VALUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

#[test]
fn discovers_credentials_then_masks_echoes_across_report_copies() {
    let config = ReportRedaction {
        headers: vec!["x-company-auth".into()],
        fields: vec!["pin".into()],
        json_paths: vec!["$.profile.identifier".into()],
        text_patterns: vec![r"account-\d+".into()],
    };
    let mut redactor = Redactor::new(&config).unwrap();
    // Ephemeral fixture values exercise userinfo masking without fixed
    // credentials in source. This URL is never used to connect to a service.
    let userinfo_id = generated_fixture_value();
    let userinfo_material = generated_fixture_value();
    let mut credential_url =
        url::Url::parse("https://example.test/items?token=URL_QUERY_SENTINEL&page=2").unwrap();
    credential_url.set_username(&userinfo_id).unwrap();
    credential_url
        .set_password(Some(&userinfo_material))
        .unwrap();
    let mut evidence = json!({
        "headers": {
            "Authorization": "Bearer TOKEN_SENTINEL",
            "Proxy-Authorization": ["Basic PROXY_SENTINEL"],
            "Set-Cookie": "opaque=COOKIE_SENTINEL; HttpOnly",
            "X-Company-Auth": "COMPANY_SENTINEL",
            "content-type": "application/json"
        },
        "query": [{"name":"api_key", "value":"QUERY_SENTINEL"}, {"name":"page", "value":"2"}],
        "body_json": {"password":"PASSWORD_SENTINEL", "pin": "PIN_SENTINEL", "safe":"visible"},
        "body": "{\"password\":\"PASSWORD_SENTINEL\",\"safe\":\"visible\"}",
        "profile": {"identifier":"PATH_SENTINEL", "display_name":"Ada"},
        "url": credential_url.as_str(),
        "raw_form": "safe=visible&password=FORM_SENTINEL",
        "encoded_form": "%70assword=FORM%20ENCODED%20SENTINEL&safe=visible",
        "raw_headers": "Authorization: Bearer RAW_AUTH_SENTINEL\nCookie: opaque=RAW_COOKIE_SENTINEL",
        "diagnostic":"account-123456"
    });
    let mut echoes = json!({"error": format!("TOKEN_SENTINEL PROXY_SENTINEL COOKIE_SENTINEL COMPANY_SENTINEL QUERY_SENTINEL PASSWORD_SENTINEL PIN_SENTINEL PATH_SENTINEL {userinfo_id} {userinfo_material} URL_QUERY_SENTINEL FORM_SENTINEL RAW_AUTH_SENTINEL RAW_COOKIE_SENTINEL FORM ENCODED SENTINEL account-123456")});
    redactor.discover(&evidence);
    redactor.discover(&echoes);
    redactor.sanitize(&mut echoes);
    redactor.sanitize(&mut evidence);
    assert!(!evidence.to_string().contains("SENTINEL"), "{evidence}");
    assert!(!echoes.to_string().contains("SENTINEL"), "{echoes}");
    for original in [&userinfo_id, &userinfo_material] {
        assert!(!evidence.to_string().contains(original), "{evidence}");
        assert!(!echoes.to_string().contains(original), "{echoes}");
    }
    assert!(!echoes.to_string().contains("account-123456"));
    assert_eq!(evidence["body_json"]["safe"], "visible");
    assert_eq!(evidence["headers"]["content-type"], "application/json");
    assert_eq!(evidence["profile"]["display_name"], "Ada");
    assert_eq!(evidence["query"][1]["value"], "2");
    assert!(evidence["url"].as_str().unwrap().contains("page=2"));
}

#[test]
fn masks_sensitive_diff_paths_and_raw_payloads_without_changing_inputs() {
    let original = json!({
        "diffs":[{"path":"$.nested.password", "expected":123, "actual":456},
                 {"path":"$.safe", "expected":"expected", "actual":"actual"}],
        "near_misses":[{"actual":{"token":"UNICODE_🔑_SENTINEL", "name":"Ada"}, "diffs":[]}],
        "body":"{\"password\":\"SPACE SECRET SENTINEL\",\"name\":\"Ada\"}",
        "error":"password='QUOTED SPACE SENTINEL'"
    });
    let mut output = original.clone();
    let mut redactor = Redactor::new(&ReportRedaction::default()).unwrap();
    redactor.sanitize(&mut output);
    assert_eq!(original["diffs"][0]["expected"], 123);
    assert_eq!(output["diffs"][0]["expected"], "[REDACTED]");
    assert_eq!(output["diffs"][0]["actual"], "[REDACTED]");
    assert_eq!(output["diffs"][1]["actual"], "actual");
    assert!(!output.to_string().contains("SENTINEL"), "{output}");
}

#[test]
fn masks_existing_run_shapes_and_junit_diagnostics() {
    let mut run = RunResult {
        schema_version: 1,
        environment: "local".into(),
        duration_ms: 15,
        tests: vec![TestResult {
            name: "test".into(),
            status: TestStatus::Failed,
            skip_reason: None,
            error: Some("error ECHO_SENTINEL".into()),
            steps: vec![StepResult {
                name: "call".into(),
                status: TestStatus::Failed,
                response: Some(ResponseSummary {
                    status: 200,
                    elapsed_ms: 10,
                    body: json!({"password":"ECHO_SENTINEL", "ok":true}),
                }),
                checks: VerifyOutcome::default(),
                attempts: 2,
            }],
            verify: VerifyOutcome {
                checks: vec![CheckResult::Fail(CheckFailure {
                    description: "expected DIFF_SENTINEL; got ACTUAL_SENTINEL".into(),
                    yaml_path: "verify.password".into(),
                    expected: json!("DIFF_SENTINEL"),
                    kind: FailureKind::ValueMismatch {
                        diffs: vec![FieldDiff {
                            path: "$.password".into(),
                            expected: json!("DIFF_SENTINEL"),
                            actual: json!("ACTUAL_SENTINEL"),
                        }],
                    },
                    near_misses: vec![NearMiss {
                        actual: json!({"token":"NEAR_SENTINEL"}),
                        diffs: vec![],
                        score: 0.5,
                    }],
                    attempts: 3,
                    elapsed_ms: 14,
                })],
            },
            seed_receipts: vec!["seeded ECHO_SENTINEL".into()],
            captures: json!({"token":"CAPTURE_SENTINEL", "public":"visible"})
                .as_object()
                .unwrap()
                .clone(),
            recorded_calls: vec![
                json!({"request":{"headers":{"Authorization":"Bearer CALL_SENTINEL"}, "body":"{\"token\":\"CALL_SENTINEL\"}"}}),
            ],
            duration_ms: 15,
            flow: Some("flow".into()),
        }],
    };
    let mut redactor = Redactor::new(&ReportRedaction::default()).unwrap();
    redactor.sanitize_run(&mut run);
    let rendered = serde_json::to_string(&run).unwrap();
    assert!(!rendered.contains("SENTINEL"), "{rendered}");
    assert!(!to_junit_xml(&run).contains("SENTINEL"));
    assert_eq!(run.schema_version, 1);
    assert_eq!(run.duration_ms, 15);
    assert_eq!(run.tests[0].steps[0].attempts, 2);
    assert_eq!(run.tests[0].steps[0].response.as_ref().unwrap().status, 200);
    assert_eq!(run.tests[0].captures["public"], "visible");
    assert_eq!(run.exit_code(), 1);
}

#[test]
fn invalid_selectors_and_patterns_fail_before_execution() {
    for config in [
        ReportRedaction {
            json_paths: vec!["$.broken[".into()],
            ..ReportRedaction::default()
        },
        ReportRedaction {
            text_patterns: vec!["(".into()],
            ..ReportRedaction::default()
        },
        ReportRedaction {
            text_patterns: vec![".*".into()],
            ..ReportRedaction::default()
        },
        ReportRedaction {
            headers: vec![String::new()],
            ..ReportRedaction::default()
        },
    ] {
        assert!(Redactor::new(&config).is_err());
    }
}

#[test]
fn unchanged_public_text_keeps_exact_format_and_percent_encoded_credentials_are_masked() {
    let mut redactor = Redactor::new(&ReportRedaction::default()).unwrap();
    // Derive percent escapes from generated material so decoded diagnostic
    // echoes remain covered without a fixed username/password pair.
    let userinfo_id = generated_fixture_value();
    let userinfo_material = generated_fixture_value();
    let encoded_material = userinfo_material.replace('-', "%2D");
    let mut credential_url =
        url::Url::parse("postgres://localhost/data?password=QUERY%5FSENTINEL&mode=read").unwrap();
    credential_url.set_username(&userinfo_id).unwrap();
    credential_url
        .set_password(Some(&encoded_material))
        .unwrap();
    assert_eq!(credential_url.password(), Some(encoded_material.as_str()));
    assert!(credential_url.as_str().contains("%2D"));
    let mut value = json!({
        "raw":"{ \"name\": \"Ada\" }",
        "url":credential_url.as_str(),
        "error":format!("{userinfo_material} QUERY_SENTINEL")
    });
    redactor.sanitize(&mut value);
    assert_eq!(value["raw"], "{ \"name\": \"Ada\" }");
    assert!(!value.to_string().contains("SENTINEL"), "{value}");
    for original in [&userinfo_id, &userinfo_material, &encoded_material] {
        assert!(!value.to_string().contains(original), "{value}");
    }
    assert!(value["url"].as_str().unwrap().contains("mode=read"));
    assert_eq!(value["error"], "[REDACTED] [REDACTED]");
}

#[test]
fn custom_paths_mask_numeric_and_nested_payload_values() {
    let mut redactor = Redactor::new(&ReportRedaction {
        json_paths: vec!["$.items[*].identifier".into()],
        ..ReportRedaction::default()
    })
    .unwrap();
    let mut report: Value = json!({"actions":[{"details":{
        "body":{"items":[{"identifier":123, "label":"public"}]},
        "diffs":[{"path":"$.items[0].identifier", "expected":456, "actual":123}]
    }}]});
    redactor.sanitize(&mut report);
    assert_eq!(
        report["actions"][0]["details"]["body"]["items"][0]["identifier"],
        "[REDACTED]"
    );
    assert_eq!(
        report["actions"][0]["details"]["body"]["items"][0]["label"],
        "public"
    );
    assert_eq!(
        report["actions"][0]["details"]["diffs"][0]["expected"],
        "[REDACTED]"
    );
    assert_eq!(
        report["actions"][0]["details"]["diffs"][0]["actual"],
        "[REDACTED]"
    );
}

#[test]
fn short_credentials_do_not_corrupt_report_status_phase_or_identifiers() {
    let mut redactor = Redactor::new(&ReportRedaction::default()).unwrap();
    let mut value = json!({
        "token":"a", "password":"1", "secret":"true",
        "status":"PASSED", "phase":"preparation", "id":"stage-1",
        "description":"value a is true; step 1", "attempts":1
    });
    redactor.sanitize(&mut value);
    assert_eq!(value["token"], "[REDACTED]");
    assert_eq!(value["password"], "[REDACTED]");
    assert_eq!(value["secret"], "[REDACTED]");
    assert_eq!(value["status"], "PASSED");
    assert_eq!(value["phase"], "preparation");
    assert_eq!(value["id"], "stage-1");
    assert_eq!(value["description"], "value a is true; step 1");
    assert_eq!(value["attempts"], 1);
}

#[test]
fn evidence_only_sensitive_captures_mask_raw_body_and_error_echoes() {
    let mut redactor = Redactor::new(&ReportRedaction::default()).unwrap();
    let mut execution = json!({"events":[
        {"phase":"http", "status":"PASSED", "details":{"response":{"body":"PAYLOAD_TOKEN_SENTINEL CLIENT_SECRET_SENTINEL"}}},
        {"phase":"capture", "subject":"token", "status":"PASSED", "details":{"value":"PAYLOAD_TOKEN_SENTINEL"}},
        {"phase":"capture", "subject":"client_secret", "status":"PASSED", "details":{"value":"CLIENT_SECRET_SENTINEL"}},
        {"phase":"capture", "subject":"public_id", "status":"PASSED", "details":{"value":"public-value"}},
        {"phase":"finalization", "status":"PASSED", "details":{"error":"PAYLOAD_TOKEN_SENTINEL CLIENT_SECRET_SENTINEL"}}
    ]});
    redactor.sanitize(&mut execution);
    assert!(!execution.to_string().contains("SENTINEL"), "{execution}");
    assert_eq!(execution["events"][1]["details"]["value"], "[REDACTED]");
    assert_eq!(execution["events"][2]["details"]["value"], "[REDACTED]");
    assert_eq!(execution["events"][3]["details"]["value"], "public-value");
    assert_eq!(execution["events"][0]["status"], "PASSED");
}

#[test]
fn execution_and_metadata_preserve_machine_envelopes_even_when_tokens_match_them() {
    let mut redactor = Redactor::new(&ReportRedaction {
        fields: vec!["status".into(), "id".into()],
        ..ReportRedaction::default()
    })
    .unwrap();
    let mut execution = json!({"flow_stage":0, "exports":{"public":"PASSED"}, "events":[
        {"phase":"http", "subject":"http request", "status":"PASSED", "step_index":0, "start_offset_ms":0, "duration_ms":5, "attempts":1,
         "details":{"body":{"token":"PASSED", "password":"http", "client_secret":"item-1", "id":"PAYLOAD_ID_SENTINEL", "status":"PAYLOAD_STATUS_SENTINEL", "safe":"visible"}}},
        {"phase":"capture", "subject":"token", "status":"PASSED", "details":{"value":"PASSED"}}
    ]});
    let mut metadata = json!({
        "started_at":"2026-10-03T00:00:00Z", "shuffle_seed":1,
        "items":[{"id":"item-1", "kind":"test", "name":"item-1 label", "description":"http passes", "reset":"once", "on_failure":"continue"}],
        "tests":[{"id":"item-1-stage-1", "item_id":"item-1", "name":"name", "stage_index":0, "result_index":0, "execution_index":0,
            "export":["token"], "step_names":["first"], "tags":["public"], "with":{"status":"STAGE_STATUS_SENTINEL", "id":"STAGE_ID_SENTINEL", "safe":"visible"}}]
    });
    redactor.discover_execution(&execution);
    redactor.discover_metadata(&metadata);
    redactor.sanitize_execution(&mut execution);
    redactor.sanitize_metadata(&mut metadata);
    assert_eq!(execution["events"][0]["status"], "PASSED");
    assert_eq!(execution["events"][0]["phase"], "http");
    assert_eq!(execution["events"][0]["attempts"], 1);
    assert_eq!(execution["events"][0]["step_index"], 0);
    assert_eq!(
        execution["events"][0]["details"]["body"]["status"],
        "[REDACTED]"
    );
    assert_eq!(
        execution["events"][0]["details"]["body"]["id"],
        "[REDACTED]"
    );
    assert_eq!(execution["events"][1]["details"]["value"], "[REDACTED]");
    assert_eq!(metadata["items"][0]["id"], "item-1");
    assert_eq!(metadata["items"][0]["kind"], "test");
    assert_eq!(metadata["items"][0]["reset"], "once");
    assert_eq!(metadata["items"][0]["on_failure"], "continue");
    assert_eq!(metadata["tests"][0]["item_id"], "item-1");
    assert_eq!(metadata["tests"][0]["id"], "item-1-stage-1");
    assert_eq!(metadata["tests"][0]["export"], json!(["token"]));
    assert_eq!(metadata["tests"][0]["with"]["status"], "[REDACTED]");
    assert_eq!(metadata["tests"][0]["with"]["id"], "[REDACTED]");
    assert_eq!(metadata["items"][0]["name"], "[REDACTED] label");
    assert_eq!(metadata["items"][0]["description"], "[REDACTED] passes");
    assert_eq!(metadata["shuffle_seed"], 1);
    assert_eq!(metadata["started_at"], "2026-10-03T00:00:00Z");
    assert!(!execution.to_string().contains("SENTINEL"));
    assert!(!metadata.to_string().contains("SENTINEL"));
}

#[test]
fn numeric_credentials_are_masked_in_derived_diagnostics() {
    let mut redactor = Redactor::new(&ReportRedaction {
        fields: vec!["pin".into()],
        ..ReportRedaction::default()
    })
    .unwrap();
    let mut value = json!({"pin":123456, "error":"PIN was 123456", "attempts":1});
    redactor.sanitize(&mut value);
    assert_eq!(value["pin"], "[REDACTED]");
    assert_eq!(value["error"], "PIN was [REDACTED]");
    assert_eq!(value["attempts"], 1);
}

#[test]
fn known_credentials_are_masked_in_encoded_url_and_form_echoes() {
    let original = json!({
        "headers":{"Authorization":"Bearer ABC-SECRET-SENTINEL"},
        "url":"https://example.test/?echo=ABC%2DSECRET%2DSENTINEL&page=2&echo=visible",
        "form":"echo=ABC%2DSECRET%2DSENTINEL&safe=visible",
        "public_url":"https://example.test/?echo=public%2Dvalue&page=2",
        "public_form":"echo=public%2Dvalue&safe=visible"
    });
    let mut value = original.clone();
    let mut redactor = Redactor::new(&ReportRedaction::default()).unwrap();
    redactor.sanitize(&mut value);
    assert!(!value.to_string().contains("SENTINEL"), "{value}");
    let url = value["url"].as_str().unwrap();
    assert!(url.contains("page=2"));
    assert!(url.contains("echo=visible"));
    assert!(value["form"].as_str().unwrap().contains("safe=visible"));
    assert_eq!(value["public_url"], original["public_url"]);
    assert_eq!(value["public_form"], original["public_form"]);
    assert!(original["url"]
        .as_str()
        .unwrap()
        .contains("ABC%2DSECRET%2DSENTINEL"));
}

#[test]
fn encoded_credentials_in_prose_and_multiline_payloads_are_masked() {
    let secret = "alias:a/b?c=d&e+f secret";
    let mut redactor = Redactor::new(&ReportRedaction::default()).unwrap();
    redactor.discover(&json!({"headers":{"X-Api-Key":secret}}));
    let mut value = json!({
        "name":"Report for alias%3Aa%2Fb%3Fc%3Dd%26e%2Bf%20secret",
        "body":"echo:\npublic_echo=alias%3Aa%2Fb%3Fc%3Dd%26e%2Bf+secret\npublic=visible"
    });
    redactor.sanitize(&mut value);
    assert_eq!(value["name"], "Report for [REDACTED]");
    assert_eq!(
        value["body"],
        "echo:\npublic_echo=[REDACTED]\npublic=visible"
    );
}

#[test]
fn sensitive_ancestors_mask_nested_and_array_diffs_regardless_of_value_length() {
    let mut redactor = Redactor::new(&ReportRedaction {
        json_paths: vec!["$.profile.private".into()],
        ..ReportRedaction::default()
    })
    .unwrap();
    let mut value = json!({
        "body":{"password":["a"], "credentials":{"pin":1234}, "profile":{"private":{"pin":42}}},
        "diffs":[
            {"path":"$.password[0]", "expected":"b", "actual":"a"},
            {"path":"$.credentials.pin", "expected":4567, "actual":1234},
            {"path":"$.profile.private.pin", "expected":43, "actual":42},
            {"path":"$.public.pin", "expected":8, "actual":9}
        ]
    });
    redactor.sanitize(&mut value);
    for index in 0..3 {
        assert_eq!(value["diffs"][index]["expected"], "[REDACTED]");
        assert_eq!(value["diffs"][index]["actual"], "[REDACTED]");
    }
    assert_eq!(value["diffs"][3]["expected"], 8);
    assert_eq!(value["diffs"][3]["actual"], 9);
}

#[test]
fn known_long_credential_prefixes_are_masked_at_core_preview_boundaries() {
    let secret = "LONG_SECRET_SENTINEL".repeat(200);
    let mut redactor = Redactor::new(&ReportRedaction::default()).unwrap();
    redactor.discover(&json!({"headers":{"Authorization":format!("Bearer {secret}")}}));
    let mut value = json!({
        "summary": format!("{}…", &secret[..2000]),
        "diff": format!("response: {}…", &secret[..490]),
        "ordinary_prefix": "LONG_SECRET_SENTINEL…",
        "common_short_fragment":format!("{}LONG_…", "x".repeat(495)),
        "public_preview":format!("{}…", "visible ".repeat(250)),
    });
    redactor.sanitize(&mut value);
    assert_eq!(value["summary"], "[REDACTED]…");
    assert_eq!(value["diff"], "response: [REDACTED]…");
    assert_eq!(value["ordinary_prefix"], "LONG_SECRET_SENTINEL…");
    assert!(value["common_short_fragment"]
        .as_str()
        .unwrap()
        .ends_with("LONG_…"));
    assert_eq!(
        value["public_preview"],
        format!("{}…", "visible ".repeat(250))
    );
}

#[test]
fn long_credential_preview_masking_reaches_json_and_junit_without_changing_matches() {
    let secret = "UNICODE_🔑_LONG_SECRET_SENTINEL".repeat(200);
    let original_response = format!("response: {secret}");
    let truncate = |limit| {
        let mut end = limit;
        while !original_response.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &original_response[..end])
    };
    let mut test = TestResult::skipped("long credential", "unused".into());
    test.status = TestStatus::Failed;
    test.skip_reason = None;
    test.steps.push(StepResult {
        name: "response".into(),
        status: TestStatus::Failed,
        response: Some(ResponseSummary {
            status: 200,
            elapsed_ms: 1,
            body: json!(truncate(2000)),
        }),
        checks: VerifyOutcome {
            checks: vec![CheckResult::Fail(CheckFailure::new(
                "raw body mismatch",
                "steps.response.expect.body",
                json!("public expected"),
                FailureKind::ValueMismatch {
                    diffs: vec![FieldDiff {
                        path: "body".into(),
                        expected: json!("public expected"),
                        actual: json!(truncate(500)),
                    }],
                },
            ))],
        },
        attempts: 1,
    });
    let mut run = RunResult {
        schema_version: 1,
        environment: "local".into(),
        tests: vec![test],
        duration_ms: 1,
    };
    let mut redactor = Redactor::new(&ReportRedaction::default()).unwrap();
    redactor.discover(&json!({"headers":{"Authorization":format!("Bearer {secret}")}}));
    redactor.sanitize_run(&mut run);
    assert!(!serde_json::to_string(&run)
        .unwrap()
        .contains("SECRET_SENTINEL"));
    assert!(!to_junit_xml(&run).contains("SECRET_SENTINEL"));
    assert!(original_response.contains("SECRET_SENTINEL"));
    assert_eq!(run.tests[0].steps[0].response.as_ref().unwrap().status, 200);
    assert_eq!(run.exit_code(), 1);
}
