use regex::Regex;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use vault_dsl::ReportRedaction;
use vault_report::{write_html, HtmlReport, Redactor};

const PAGES: &str = "vault-report-pages";
const OWNERSHIP: &str = ".vault-report-pages.json";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "vault-html-index-{}-{nonce}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.0.join(relative)).unwrap()
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn suite_report() -> HtmlReport {
    HtmlReport {
        schema_version: 1,
        run: json!({
            "schema_version": 1,
            "environment": "ci",
            "duration_ms": 200,
            "tests": [
                {"name":"shared check", "status":"PASSED", "duration_ms":12, "captures":{"public":"merchant-second"}},
                {"name":"shared check", "status":"FAILED", "duration_ms":13, "error":"shop-only failure"},
                {"name":"shared check", "status":"ERRORED", "duration_ms":14, "error":"provider-only error"},
                {"name":"shared check", "status":"PASSED", "duration_ms":11, "captures":{"public":"merchant-first"}},
                {"name":"shared check", "status":"SKIPPED", "duration_ms":0, "skip_reason":"zone-only reason"},
                {"name":"standalone check", "status":"PASSED", "duration_ms":15}
            ]
        }),
        metadata: json!({
            "suite":"suite", "started_at":"2026-10-04T00:00:00Z",
            "items":[
                {"id":"merchant", "name":"merchant flow", "kind":"flow", "reset":"once"},
                {"id":"shop", "name":"shop flow", "kind":"flow"},
                {"id":"provider", "name":"provider flow", "kind":"flow"},
                {"id":"zone", "name":"zone flow", "kind":"flow"},
                {"id":"rule", "name":"rule flow", "kind":"flow"},
                {"id":"standalone", "name":"standalone check", "kind":"test"}
            ],
            "tests":[
                {"id":"merchant-first", "item_id":"merchant", "name":"shared check", "flow":"merchant flow", "stage_index":0, "result_index":3, "execution_index":4},
                {"id":"merchant-second", "item_id":"merchant", "name":"shared check", "flow":"merchant flow", "stage_index":1, "result_index":0, "execution_index":1},
                {"id":"shop-first", "item_id":"shop", "name":"shared check", "flow":"shop flow", "stage_index":0, "result_index":1, "execution_index":2},
                {"id":"shop-not-reached", "item_id":"shop", "name":"shared check", "flow":"shop flow", "stage_index":1, "result_index":null, "execution_index":null},
                {"id":"provider-first", "item_id":"provider", "name":"shared check", "flow":"provider flow", "stage_index":0, "result_index":2, "execution_index":3},
                {"id":"zone-first", "item_id":"zone", "name":"shared check", "flow":"zone flow", "stage_index":0, "result_index":4, "execution_index":0},
                {"id":"rule-not-reached", "item_id":"rule", "name":"shared check", "flow":"rule flow", "stage_index":0, "result_index":null, "execution_index":null},
                {"id":"standalone-test", "item_id":"standalone", "name":"standalone check", "result_index":5, "execution_index":5}
            ]
        }),
        executions: ["zone-only", "merchant-second", "shop-only", "provider-only", "merchant-first", "standalone-only"]
            .into_iter()
            .map(|subject| json!({"events":[{"phase":"step", "subject":subject, "status":"PASSED", "details":{"response":{"body":subject}}}]}))
            .collect(),
    }
}

fn one_flow(name: &str, status: &str) -> HtmlReport {
    HtmlReport {
        schema_version: 1,
        run: json!({"environment":"ci", "duration_ms":12, "tests":[{"name":"check", "status":status, "duration_ms":12}]}),
        metadata: json!({
            "items":[{"id":"only", "name":name, "kind":"flow"}],
            "tests":[{"id":"only-stage", "item_id":"only", "name":"check", "flow":name, "stage_index":0, "result_index":0, "execution_index":0}]
        }),
        executions: vec![json!({"events":[]})],
    }
}

fn write(root: &Path, filename: &str, report: &HtmlReport) {
    write_html(report, root.join(filename).to_str().unwrap()).unwrap();
}

fn embedded_data(html: &str) -> Value {
    let tag = "<script id=\"vault-report-data\" type=\"application/json\">";
    let start = html.find(tag).unwrap() + tag.len();
    let end = start + html[start..].find("</script>").unwrap();
    serde_json::from_str(&html[start..end]).unwrap()
}

fn index_rows(html: &str) -> Vec<String> {
    Regex::new(r"(?s)<tr\b[^>]*>.*?</tr>")
        .unwrap()
        .find_iter(html)
        .map(|row| row.as_str().to_owned())
        .filter(|row| row.contains("data-status="))
        .collect()
}

fn html_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(html_files(&path));
        } else if path
            .extension()
            .is_some_and(|extension| extension == "html")
        {
            files.push(path);
        }
    }
    files
}

fn assert_external_stylesheet(html: &str, href: &str) {
    assert!(
        html.contains(&format!("<link rel=\"stylesheet\" href=\"{href}\">")),
        "missing external stylesheet {href}"
    );
    assert_eq!(html.matches("rel=\"stylesheet\"").count(), 1);
    assert!(!Regex::new(r"(?i)<style\b|\sstyle\s*=")
        .unwrap()
        .is_match(html));
}

fn published_outputs(directory: &TempDirectory) -> Vec<(&'static str, String)> {
    [
        "report.html",
        "index.html",
        "vault-report-pages/flow-001.html",
        "vault-report-pages/.vault-report-pages.json",
    ]
    .into_iter()
    .map(|name| (name, directory.read(name)))
    .collect()
}

#[test]
fn one_invocation_lists_five_flows_and_standalone_in_execution_order() {
    let directory = TempDirectory::new();
    let report = suite_report();
    write(directory.path(), "report.html", &report);
    let index = directory.read("index.html");
    let rows = index_rows(&index);
    let expected = [
        ("merchant flow", "PASSED", "flow-001.html"),
        ("shop flow", "FAILED", "flow-002.html"),
        ("provider flow", "ERRORED", "flow-003.html"),
        ("zone flow", "SKIPPED", "flow-004.html"),
        ("rule flow", "NOT_RUN", "flow-005.html"),
        ("standalone check", "PASSED", "test-006.html"),
    ];
    assert_eq!(rows.len(), expected.len());
    for (row, (name, status, filename)) in rows.iter().zip(expected) {
        assert!(row.contains(name), "{row}");
        assert!(row.contains(&format!("data-status=\"{status}\"")), "{row}");
        assert!(row.contains(&format!("{PAGES}/{filename}")), "{row}");
        assert!(directory.path().join(PAGES).join(filename).is_file());
    }
    let aggregate = embedded_data(&directory.read("report.html"));
    assert_eq!(aggregate["run"], report.run);
    assert_eq!(aggregate["metadata"]["tests"], report.metadata["tests"]);
    assert_eq!(aggregate["executions"], json!(report.executions));
    assert!(!directory.path().join(".vault-report-index.json").exists());
    for page in html_files(directory.path()) {
        let href = if page.parent() == Some(directory.path()) {
            "vault-report-pages/style.css"
        } else {
            "style.css"
        };
        assert_external_stylesheet(&std::fs::read_to_string(page).unwrap(), href);
    }
    let css = directory.read("vault-report-pages/style.css");
    assert!(css.contains(":root{color-scheme:"));
    assert!(css.contains(".report-index-table"));
    assert!(css.contains(".masthead"));
}

#[test]
fn detail_pages_remap_repeated_occurrences_and_isolate_each_items_evidence() {
    let directory = TempDirectory::new();
    write(directory.path(), "report.html", &suite_report());
    let html = directory.read("vault-report-pages/flow-001.html");
    let merchant = embedded_data(&html);
    assert_eq!(merchant["metadata"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(merchant["metadata"]["items"][0]["id"], "merchant");
    assert_eq!(
        merchant["metadata"]["report_page"]["title"],
        "merchant flow"
    );
    assert_eq!(merchant["run"]["duration_ms"], 23);
    let tests = merchant["metadata"]["tests"].as_array().unwrap();
    assert_eq!(tests.len(), 2);
    for (ordinal, expected) in ["merchant-first", "merchant-second"].iter().enumerate() {
        let descriptor = &tests[ordinal];
        assert_eq!(descriptor["id"], *expected);
        assert_eq!(descriptor["name"], "shared check");
        assert_eq!(descriptor["stage_index"], ordinal);
        assert_eq!(descriptor["result_index"], ordinal);
        assert_eq!(descriptor["execution_index"], ordinal);
        assert_eq!(
            merchant["run"]["tests"][ordinal]["captures"]["public"],
            *expected
        );
        assert_eq!(
            merchant["executions"][ordinal]["events"][0]["subject"],
            *expected
        );
    }
    assert_eq!(merchant["run"]["tests"].as_array().unwrap().len(), 2);
    assert_eq!(merchant["executions"].as_array().unwrap().len(), 2);
    for other in ["shop-only", "provider-only", "zone-only", "standalone-only"] {
        assert!(
            !html.contains(other),
            "another item's evidence leaked: {other}"
        );
    }
    assert!(html.contains("href=\"../index.html\""));

    let shop = embedded_data(&directory.read("vault-report-pages/flow-002.html"));
    assert_eq!(shop["metadata"]["tests"].as_array().unwrap().len(), 2);
    assert_eq!(shop["run"]["tests"].as_array().unwrap().len(), 1);
    assert_eq!(shop["executions"].as_array().unwrap().len(), 1);
    assert_eq!(shop["metadata"]["tests"][0]["result_index"], 0);
    assert_eq!(shop["metadata"]["tests"][0]["execution_index"], 0);
    assert_eq!(shop["metadata"]["tests"][1]["result_index"], Value::Null);
    assert_eq!(shop["metadata"]["tests"][1]["execution_index"], Value::Null);
    assert_eq!(shop["run"]["tests"][0]["status"], "FAILED");

    let unrun = embedded_data(&directory.read("vault-report-pages/flow-005.html"));
    assert_eq!(unrun["run"]["tests"], json!([]));
    assert_eq!(unrun["executions"], json!([]));
    assert_eq!(unrun["metadata"]["tests"][0]["result_index"], Value::Null);
}

#[test]
fn rerun_replaces_entries_and_removes_only_previously_generated_pages() {
    let directory = TempDirectory::new();
    write(directory.path(), "report.html", &suite_report());
    let unrelated = directory.path().join(PAGES).join("flow-999.html");
    std::fs::write(&unrelated, "unrelated page").unwrap();
    std::fs::write(directory.path().join("keep.txt"), "keep me").unwrap();
    write(
        directory.path(),
        "report.html",
        &one_flow("replacement flow", "FAILED"),
    );
    let index = directory.read("index.html");
    assert_eq!(index_rows(&index).len(), 1);
    assert_eq!(index.matches("vault-report-pages/flow-001.html").count(), 1);
    assert!(index_rows(&index)[0].contains("data-status=\"FAILED\""));
    assert!(index.contains("replacement flow"));
    for old in [
        "merchant flow",
        "shop flow",
        "provider flow",
        "zone flow",
        "rule flow",
        "standalone check",
    ] {
        assert!(!index.contains(old), "stale entry: {old}");
    }
    for old in [
        "flow-002.html",
        "flow-003.html",
        "flow-004.html",
        "flow-005.html",
        "test-006.html",
    ] {
        assert!(
            !directory.path().join(PAGES).join(old).exists(),
            "stale page: {old}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(unrelated).unwrap(),
        "unrelated page"
    );
    assert_eq!(directory.read("keep.txt"), "keep me");
    let ownership: Value =
        serde_json::from_str(&directory.read(&format!("{PAGES}/{OWNERSHIP}"))).unwrap();
    assert_eq!(ownership["files"], json!(["flow-001.html", "style.css"]));
    assert!(directory.path().join(PAGES).join("style.css").is_file());
}

#[test]
fn separate_invocations_replace_the_index_instead_of_accumulating_history() {
    let directory = TempDirectory::new();
    write(
        directory.path(),
        "earlier.html",
        &one_flow("earlier flow", "PASSED"),
    );
    let earlier = directory.read("earlier.html");
    write(
        directory.path(),
        "latest.html",
        &one_flow("latest flow", "FAILED"),
    );
    let index = directory.read("index.html");
    assert_eq!(index_rows(&index).len(), 1);
    assert!(index.contains("latest flow"));
    assert!(!index.contains("earlier flow"));
    assert_eq!(directory.read("earlier.html"), earlier);
}

#[test]
fn custom_paths_and_hostile_labels_remain_portable_after_moving_the_bundle() {
    let directory = TempDirectory::new();
    let original = directory.path().join("original/nested");
    let name = "<img src=x onerror=alert(1)> & \"orders\" café";
    let filename = "report #1?100%\" café.html";
    write(&original, filename, &one_flow(name, "PASSED"));
    let relocated = directory.path().join("relocated");
    std::fs::rename(&original, &relocated).unwrap();
    let index = std::fs::read_to_string(relocated.join("index.html")).unwrap();
    assert!(!index.contains("<img src=x"));
    assert!(index.contains("&lt;img src=x onerror=alert(1)&gt;"));
    assert!(index.contains("&amp;"));
    let link = Regex::new(r#"href="([^"]+)""#).unwrap();
    for page in html_files(&relocated) {
        let html = std::fs::read_to_string(&page).unwrap();
        assert!(!html.contains(original.to_str().unwrap()));
        assert!(!html.contains("fetch("));
        assert!(!html.contains("<script src="));
        let stylesheet = if page.parent() == Some(relocated.as_path()) {
            "vault-report-pages/style.css"
        } else {
            "style.css"
        };
        assert_external_stylesheet(&html, stylesheet);
        for capture in link.captures_iter(&html) {
            let target = url::Url::from_file_path(&page)
                .unwrap()
                .join(&capture[1])
                .unwrap();
            assert_eq!(
                target.scheme(),
                "file",
                "non-offline link in {page:?}: {target}"
            );
            assert!(
                target.to_file_path().unwrap().is_file(),
                "broken link in {page:?}: {target}"
            );
        }
    }
    let data = embedded_data(
        &std::fs::read_to_string(relocated.join(PAGES).join("flow-001.html")).unwrap(),
    );
    assert_eq!(data["metadata"]["items"][0]["name"], name);
}

#[test]
fn explicit_index_html_combines_navigation_and_aggregate_without_duplicate_entries() {
    let directory = TempDirectory::new();
    let report = suite_report();
    write(directory.path(), "index.html", &report);
    let index = directory.read("index.html");
    assert_eq!(index_rows(&index).len(), 6);
    assert_external_stylesheet(&index, "vault-report-pages/style.css");
    assert_eq!(embedded_data(&index)["run"], report.run);
    assert!(index.contains("vault-report-pages/flow-005.html"));
    assert!(index.contains("vault-report-pages/test-006.html"));
    write(
        directory.path(),
        "index.html",
        &one_flow("changed flow", "FAILED"),
    );
    let index = directory.read("index.html");
    assert_eq!(index_rows(&index).len(), 1);
    assert_external_stylesheet(&index, "vault-report-pages/style.css");
    assert_eq!(embedded_data(&index)["run"]["tests"][0]["status"], "FAILED");
    assert!(!index.contains("merchant flow"));
}

#[test]
fn pages_reuse_globally_sanitized_data_including_cross_flow_secret_echoes() {
    let directory = TempDirectory::new();
    let mut report = suite_report();
    let secret = "CROSS_FLOW_SECRET_SENTINEL";
    report.metadata["items"][0]["name"] = json!(format!("merchant {secret}"));
    report.executions[4]["events"][0]["details"]["response"]["body"] =
        json!(format!("echo {secret}"));
    report.executions[2]["events"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "phase":"capture", "subject":"token", "status":"PASSED", "details":{"value":secret}
        }));
    let mut redactor = Redactor::new(&ReportRedaction::default()).unwrap();
    redactor.discover(&report.run);
    redactor.discover_metadata(&report.metadata);
    for execution in &report.executions {
        redactor.discover_execution(execution);
    }
    redactor.sanitize(&mut report.run);
    redactor.sanitize_metadata(&mut report.metadata);
    for execution in &mut report.executions {
        redactor.sanitize_execution(execution);
    }
    write(directory.path(), "report.html", &report);
    for page in html_files(directory.path()) {
        let html = std::fs::read_to_string(&page).unwrap();
        assert!(!html.contains(secret), "secret leaked into {page:?}");
    }
    let merchant = embedded_data(&directory.read("vault-report-pages/flow-001.html"));
    assert_eq!(
        merchant["executions"][0]["events"][0]["details"]["response"]["body"],
        "echo [REDACTED]"
    );
    assert!(directory.read("index.html").contains("merchant [REDACTED]"));
}

#[test]
fn unrelated_index_or_unowned_generated_directory_is_not_overwritten() {
    let directory = TempDirectory::new();
    std::fs::write(directory.path().join("index.html"), "existing homepage").unwrap();
    let error = write_html(
        &suite_report(),
        directory.path().join("report.html").to_str().unwrap(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("index.html"));
    assert_eq!(directory.read("index.html"), "existing homepage");
    assert_external_stylesheet(
        &directory.read("report.html"),
        "vault-report-pages/style.css",
    );
    assert!(directory.path().join(PAGES).join("style.css").is_file());

    let directory = TempDirectory::new();
    std::fs::create_dir(directory.path().join(PAGES)).unwrap();
    std::fs::write(
        directory.path().join(PAGES).join("flow-001.html"),
        "unrelated page",
    )
    .unwrap();
    assert!(write_html(
        &suite_report(),
        directory.path().join("report.html").to_str().unwrap()
    )
    .is_err());
    assert_eq!(
        directory.read("vault-report-pages/flow-001.html"),
        "unrelated page"
    );
    assert!(!directory.path().join("index.html").exists());
}

#[test]
fn detail_write_failure_preserves_the_previously_published_index() {
    let directory = TempDirectory::new();
    write(
        directory.path(),
        "report.html",
        &one_flow("previous flow", "PASSED"),
    );
    let index = directory.read("index.html");
    let blocked = directory.path().join(PAGES).join("flow-002.html");
    std::fs::create_dir(&blocked).unwrap();
    let error = write_html(
        &suite_report(),
        directory.path().join("report.html").to_str().unwrap(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("flow-002.html"), "{error}");
    assert_eq!(directory.read("index.html"), index);
    assert!(blocked.is_dir());
    assert_external_stylesheet(
        &directory.read("report.html"),
        "vault-report-pages/style.css",
    );
    assert!(directory.path().join(PAGES).join("style.css").is_file());
}

#[test]
fn malformed_ownership_marker_does_not_trigger_cleanup_or_replace_index() {
    let directory = TempDirectory::new();
    write(directory.path(), "report.html", &suite_report());
    let old_index = directory.read("index.html");
    let old_report = directory.read("report.html");
    std::fs::write(directory.path().join(PAGES).join(OWNERSHIP), "{").unwrap();
    assert!(write_html(
        &one_flow("replacement", "PASSED"),
        directory.path().join("report.html").to_str().unwrap()
    )
    .is_err());
    assert_eq!(directory.read("index.html"), old_index);
    assert_eq!(directory.read("report.html"), old_report);
    assert!(directory.path().join(PAGES).join("flow-005.html").is_file());
    assert_eq!(directory.read(&format!("{PAGES}/{OWNERSHIP}")), "{");
}

#[test]
fn empty_invocation_clears_previous_entries_and_generated_pages() {
    let directory = TempDirectory::new();
    write(directory.path(), "report.html", &suite_report());
    let css = directory.read("vault-report-pages/style.css");
    let empty = HtmlReport {
        schema_version: 1,
        run: json!({"tests":[], "duration_ms":0}),
        metadata: json!({"items":[], "tests":[]}),
        executions: vec![],
    };
    write(directory.path(), "report.html", &empty);
    assert!(index_rows(&directory.read("index.html")).is_empty());
    assert!(!directory.read("index.html").contains("merchant flow"));
    assert!(html_files(&directory.path().join(PAGES)).is_empty());
    assert_eq!(directory.read("vault-report-pages/style.css"), css);
    let ownership: Value =
        serde_json::from_str(&directory.read(&format!("{PAGES}/{OWNERSHIP}"))).unwrap();
    assert_eq!(ownership["files"], json!(["style.css"]));
    assert_external_stylesheet(
        &directory.read("index.html"),
        "vault-report-pages/style.css",
    );
    assert_external_stylesheet(
        &directory.read("report.html"),
        "vault-report-pages/style.css",
    );
    assert_eq!(
        embedded_data(&directory.read("report.html"))["run"]["tests"],
        json!([])
    );
}

#[test]
fn old_page_only_ownership_upgrades_without_removing_unrelated_stylesheets() {
    let directory = TempDirectory::new();
    let pages = directory.path().join(PAGES);
    std::fs::create_dir(&pages).unwrap();
    std::fs::write(
        pages.join(OWNERSHIP),
        serde_json::to_vec(&json!({
            "schema_version":1, "files":["flow-001.html", "flow-002.html"]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(pages.join("flow-001.html"), "old first page").unwrap();
    std::fs::write(pages.join("flow-002.html"), "old second page").unwrap();
    std::fs::write(pages.join("custom.css"), "/* unrelated stylesheet */").unwrap();
    std::fs::write(
        directory.path().join("index.html"),
        "<!doctype html><!-- vault-report-index:v1 --><p>old index</p>",
    )
    .unwrap();

    write(
        directory.path(),
        "report.html",
        &one_flow("updated", "PASSED"),
    );

    assert!(pages.join("style.css").is_file());
    assert!(!pages.join("flow-002.html").exists());
    assert_eq!(
        directory.read("vault-report-pages/custom.css"),
        "/* unrelated stylesheet */"
    );
    let ownership: Value =
        serde_json::from_str(&directory.read(&format!("{PAGES}/{OWNERSHIP}"))).unwrap();
    assert_eq!(ownership["schema_version"], 1);
    assert_eq!(ownership["files"], json!(["flow-001.html", "style.css"]));
    assert_external_stylesheet(
        &directory.read("report.html"),
        "vault-report-pages/style.css",
    );
    assert_external_stylesheet(
        &directory.read("index.html"),
        "vault-report-pages/style.css",
    );
    assert_external_stylesheet(
        &directory.read("vault-report-pages/flow-001.html"),
        "style.css",
    );
}

#[test]
fn ownership_does_not_accept_arbitrary_css_for_cleanup() {
    let directory = TempDirectory::new();
    write(
        directory.path(),
        "report.html",
        &one_flow("previous", "PASSED"),
    );
    std::fs::write(directory.path().join(PAGES).join("custom.css"), "keep me").unwrap();
    std::fs::write(
        directory.path().join(PAGES).join(OWNERSHIP),
        serde_json::to_vec(&json!({
            "schema_version":1, "files":["flow-001.html", "style.css", "custom.css"]
        }))
        .unwrap(),
    )
    .unwrap();
    let before = published_outputs(&directory);
    let css = directory.read("vault-report-pages/style.css");
    assert!(write_html(
        &one_flow("replacement", "FAILED"),
        directory.path().join("report.html").to_str().unwrap()
    )
    .is_err());
    assert_eq!(published_outputs(&directory), before);
    assert_eq!(directory.read("vault-report-pages/style.css"), css);
    assert_eq!(directory.read("vault-report-pages/custom.css"), "keep me");
}

#[test]
fn unowned_stylesheet_collision_preserves_existing_outputs() {
    let directory = TempDirectory::new();
    write(
        directory.path(),
        "report.html",
        &one_flow("previous", "PASSED"),
    );
    std::fs::write(
        directory.path().join(PAGES).join(OWNERSHIP),
        serde_json::to_vec(&json!({"schema_version":1, "files":["flow-001.html"]})).unwrap(),
    )
    .unwrap();
    std::fs::write(
        directory.path().join(PAGES).join("style.css"),
        "unowned CSS",
    )
    .unwrap();
    let before = published_outputs(&directory);
    let error = write_html(
        &one_flow("replacement", "FAILED"),
        directory.path().join("report.html").to_str().unwrap(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("style.css"), "{error}");
    assert_eq!(published_outputs(&directory), before);
    assert_eq!(
        directory.read("vault-report-pages/style.css"),
        "unowned CSS"
    );
}

#[test]
fn stylesheet_write_failure_preserves_existing_outputs() {
    let directory = TempDirectory::new();
    write(
        directory.path(),
        "report.html",
        &one_flow("previous", "PASSED"),
    );
    let stylesheet = directory.path().join(PAGES).join("style.css");
    std::fs::remove_file(&stylesheet).unwrap();
    std::fs::create_dir(&stylesheet).unwrap();
    std::fs::write(stylesheet.join("keep.txt"), "directory content").unwrap();
    let before = published_outputs(&directory);
    let error = write_html(
        &one_flow("replacement", "FAILED"),
        directory.path().join("report.html").to_str().unwrap(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("style.css"), "{error}");
    assert_eq!(published_outputs(&directory), before);
    assert_eq!(
        std::fs::read_to_string(stylesheet.join("keep.txt")).unwrap(),
        "directory content"
    );
}

#[cfg(unix)]
#[test]
fn stylesheet_symlink_preserves_existing_outputs_and_external_target() {
    use std::os::unix::fs::symlink;

    let directory = TempDirectory::new();
    let outside = TempDirectory::new();
    write(
        directory.path(),
        "report.html",
        &one_flow("previous", "PASSED"),
    );
    std::fs::write(outside.path().join("style.css"), "external CSS").unwrap();
    let stylesheet = directory.path().join(PAGES).join("style.css");
    std::fs::remove_file(&stylesheet).unwrap();
    symlink(outside.path().join("style.css"), &stylesheet).unwrap();
    let before = published_outputs(&directory);
    let error = write_html(
        &one_flow("replacement", "FAILED"),
        directory.path().join("report.html").to_str().unwrap(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("style.css"), "{error}");
    assert_eq!(published_outputs(&directory), before);
    assert_eq!(outside.read("style.css"), "external CSS");
    assert!(std::fs::symlink_metadata(stylesheet)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn uppercase_index_filename_preserves_aggregate_on_case_insensitive_filesystems() {
    let directory = TempDirectory::new();
    let report = suite_report();
    write(directory.path(), "INDEX.HTML", &report);
    assert_external_stylesheet(
        &directory.read("INDEX.HTML"),
        "vault-report-pages/style.css",
    );
    assert_external_stylesheet(
        &directory.read("index.html"),
        "vault-report-pages/style.css",
    );
    assert_eq!(
        embedded_data(&directory.read("INDEX.HTML"))["run"],
        report.run
    );
    assert_eq!(index_rows(&directory.read("index.html")).len(), 6);
    write(
        directory.path(),
        "INDEX.HTML",
        &one_flow("latest flow", "FAILED"),
    );
    assert_eq!(
        embedded_data(&directory.read("INDEX.HTML"))["run"]["tests"][0]["status"],
        "FAILED"
    );
    assert_eq!(index_rows(&directory.read("index.html")).len(), 1);
    assert_external_stylesheet(
        &directory.read("INDEX.HTML"),
        "vault-report-pages/style.css",
    );
    assert_external_stylesheet(
        &directory.read("index.html"),
        "vault-report-pages/style.css",
    );
}

#[test]
fn ownership_marker_cannot_delete_paths_outside_its_generated_directory() {
    let directory = TempDirectory::new();
    write(directory.path(), "report.html", &suite_report());
    let index = directory.read("index.html");
    std::fs::write(directory.path().join("flow-999.html"), "outside file").unwrap();
    std::fs::write(
        directory.path().join(PAGES).join(OWNERSHIP),
        serde_json::to_vec(&json!({"schema_version":1,"files":["../flow-999.html"]})).unwrap(),
    )
    .unwrap();
    assert!(write_html(
        &one_flow("changed", "PASSED"),
        directory.path().join("report.html").to_str().unwrap()
    )
    .is_err());
    assert_eq!(directory.read("flow-999.html"), "outside file");
    assert_eq!(directory.read("index.html"), index);
    assert!(directory.path().join(PAGES).join("flow-005.html").is_file());
}

#[cfg(unix)]
#[test]
fn generated_directory_and_report_symlinks_cannot_overwrite_external_files() {
    use std::os::unix::fs::symlink;

    let outside = TempDirectory::new();
    std::fs::write(outside.path().join("flow-001.html"), "external content").unwrap();
    let directory = TempDirectory::new();
    symlink(outside.path(), directory.path().join(PAGES)).unwrap();
    assert!(write_html(
        &suite_report(),
        directory.path().join("report.html").to_str().unwrap()
    )
    .is_err());
    assert_eq!(outside.read("flow-001.html"), "external content");
    assert!(!outside.path().join(OWNERSHIP).exists());
    assert!(!directory.path().join("index.html").exists());

    let directory = TempDirectory::new();
    write(directory.path(), "report.html", &suite_report());
    let index = directory.read("index.html");
    let page = directory.path().join(PAGES).join("flow-001.html");
    std::fs::remove_file(&page).unwrap();
    symlink(outside.path().join("flow-001.html"), &page).unwrap();
    assert!(write_html(
        &one_flow("changed", "FAILED"),
        directory.path().join("report.html").to_str().unwrap()
    )
    .is_err());
    assert_eq!(outside.read("flow-001.html"), "external content");
    assert!(std::fs::symlink_metadata(page)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(directory.read("index.html"), index);
}

#[cfg(unix)]
#[test]
fn stale_page_cleanup_preserves_symlinks_and_their_external_targets() {
    use std::os::unix::fs::symlink;

    let directory = TempDirectory::new();
    let outside = TempDirectory::new();
    std::fs::write(outside.path().join("sentinel.html"), "external content").unwrap();
    write(directory.path(), "report.html", &suite_report());
    let stale = directory.path().join(PAGES).join("flow-005.html");
    std::fs::remove_file(&stale).unwrap();
    symlink(outside.path().join("sentinel.html"), &stale).unwrap();
    assert!(write_html(
        &one_flow("replacement", "PASSED"),
        directory.path().join("report.html").to_str().unwrap()
    )
    .is_err());
    assert!(std::fs::symlink_metadata(stale)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(outside.read("sentinel.html"), "external content");
}

#[test]
fn uppercase_index_failure_preserves_previously_published_navigation() {
    let directory = TempDirectory::new();
    write(
        directory.path(),
        "INDEX.HTML",
        &one_flow("previous flow", "PASSED"),
    );
    let index = directory.read("index.html");
    std::fs::create_dir(directory.path().join(PAGES).join("flow-002.html")).unwrap();
    assert!(write_html(
        &suite_report(),
        directory.path().join("INDEX.HTML").to_str().unwrap()
    )
    .is_err());
    assert_eq!(directory.read("index.html"), index);
}
