use serde_json::json;
use vault_core::{RunResult, TestResult, TestStatus};
use vault_report::to_junit_xml;
use vault_store::{CheckFailure, CheckResult, FailureKind, VerifyOutcome};

fn test_result(name: &str, status: TestStatus, flow: Option<&str>) -> TestResult {
    TestResult {
        name: name.into(),
        status,
        skip_reason: None,
        error: None,
        steps: vec![],
        verify: VerifyOutcome::default(),
        seed_receipts: vec![],
        captures: Default::default(),
        recorded_calls: vec![],
        duration_ms: 1,
        flow: flow.map(str::to_owned),
    }
}

#[test]
fn control_characters_and_line_breaks_stay_well_formed_xml() {
    let mut failed = test_result(
        "a\u{1b}[31m\0b\u{fffe}",
        TestStatus::Failed,
        Some("checkout"),
    );
    failed.verify = VerifyOutcome {
        checks: vec![CheckResult::Fail(CheckFailure {
            description: "l1\nl2\ttab\r\n<q> & \"dq\"".into(),
            yaml_path: "verify".into(),
            expected: json!(1),
            kind: FailureKind::MissingRow,
            near_misses: vec![],
            attempts: 1,
            elapsed_ms: 1,
        })],
    };
    let run = RunResult {
        orphans: vec![],
        reruns: vec![],
        schema_version: 1,
        environment: "local".into(),
        duration_ms: 2,
        tests: vec![failed, test_result("standalone", TestStatus::Passed, None)],
    };

    let xml = to_junit_xml(&run);
    assert!(!xml.contains('\u{1b}'), "{xml}");
    assert!(!xml.contains('\0'), "{xml}");
    assert!(!xml.contains('\u{fffe}'), "{xml}");
    assert!(
        xml.contains("name=\"a\u{fffd}[31m\u{fffd}b\u{fffd}\""),
        "{xml}"
    );
    assert!(
        xml.contains("message=\"l1&#10;l2&#9;tab&#13;&#10;&lt;q&gt; &amp; &quot;dq&quot;\""),
        "{xml}"
    );
    assert!(
        xml.contains("<testcase classname=\"checkout\" name="),
        "{xml}"
    );
    assert!(
        xml.contains("<testcase classname=\"vault\" name=\"standalone\""),
        "{xml}"
    );
}
