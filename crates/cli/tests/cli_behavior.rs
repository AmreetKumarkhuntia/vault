use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MATCHING_PATTERN: &str = "selection sentinel";
const MISSING_PATTERN: &str = "definitely-no-selection-*";
const TARGET_URL_ENV: &str = "VAULT_CLI_TEST_URL";

static TEMP_DIR_ID: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let id = TEMP_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("vault-cli-{label}-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("temporary test directory should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct HttpServer {
    address: SocketAddr,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HttpServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .expect("HTTP test server should bind an ephemeral port");
        let address = listener
            .local_addr()
            .expect("HTTP test server should have a local address");
        listener
            .set_nonblocking(true)
            .expect("HTTP test listener should become nonblocking");

        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = serve_http(stream);
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            address,
            shutdown,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_http(mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut request = Vec::with_capacity(1024);
    let mut buffer = [0_u8; 1024];
    while request.len() < 16 * 1024 {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                break;
            }
        }
    }

    let request = String::from_utf8_lossy(&request);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    if path.split('?').next() == Some("/credential-echo") {
        let header = |wanted: &str| {
            request
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case(wanted).then(|| value.trim())
                })
                .unwrap_or("")
        };
        let long = header("X-Long-Token");
        let encoded = encode_test_component(header("X-Encoded-Credential"));
        let body = format!(
            "{long}\nhttp://example.invalid/echo?public_echo={encoded}\npublic_echo={encoded}"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nx-proof: {long}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes())?;
        return stream.flush();
    }
    let (status, body) = match path.split('?').next().unwrap_or(path) {
        "/pass" | "/healthz" => ("200 OK", r#"{"status":"ok"}"#),
        "/secret"
            if request
                .to_ascii_lowercase()
                .contains("x-private: cli-custom-sentinel") =>
        {
            (
                "200 OK",
                r#"{"status":"ok","access_token":"cli-secret-sentinel","nested":{"private":"cli-nested-sentinel"},"message":"cli-pattern-hidden","public_echo":"cli-custom-sentinel","public_query":"cli-query-sentinel"}"#,
            )
        }
        _ => ("404 Not Found", r#"{"error":"not_found"}"#),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

fn encode_test_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .canonicalize()
        .expect("CLI test fixture should exist")
}

fn vault_command(sandbox: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vault"));
    command
        .current_dir(sandbox.path())
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("FORCE_COLOR")
        .env("TERM", "xterm-256color");
    command
}

fn configured_run(sandbox: &TempDir, server: &HttpServer, pattern: &str) -> Command {
    let mut command = vault_command(sandbox);
    command
        .arg("run")
        .arg(pattern)
        .arg("--suite-dir")
        .arg(fixture("reporting"))
        .env(TARGET_URL_ENV, server.url());
    command
}

fn output(command: &mut Command) -> Output {
    command.output().expect("vault subprocess should start")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn assert_no_ansi(label: &str, bytes: &[u8]) {
    assert!(
        !bytes.contains(&0x1b),
        "{label} unexpectedly contained ANSI escapes:\n{}",
        text(bytes)
    );
}

fn assert_has_ansi(label: &str, bytes: &[u8]) {
    assert!(
        bytes.contains(&0x1b),
        "{label} did not contain the expected ANSI positive control:\n{}",
        text(bytes)
    );
}

fn assert_no_ansi_output(result: &Output) {
    assert_no_ansi("stdout", &result.stdout);
    assert_no_ansi("stderr", &result.stderr);
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected exit status\nstdout:\n{}\nstderr:\n{}",
        text(&output.stdout),
        text(&output.stderr),
    );
}

fn blocked_report_path(sandbox: &TempDir, blocker_name: &str, report_name: &str) -> PathBuf {
    let blocker = sandbox.path().join(blocker_name);
    fs::write(&blocker, "not a directory").expect("report-path blocker should be created");
    blocker.join(report_name)
}

fn assert_config_reports(sandbox: &TempDir, expected_status: &str, expected_failures: usize) {
    let json_path = sandbox.path().join("config-report.json");
    let junit_path = sandbox.path().join("config-junit.xml");
    let json = fs::read_to_string(&json_path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", json_path.display()));
    let junit = fs::read_to_string(&junit_path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", junit_path.display()));

    assert!(
        json.contains(&format!(r#""status": "{expected_status}""#)),
        "JSON report:\n{json}"
    );
    assert!(
        junit.contains(&format!(r#"failures="{expected_failures}""#)),
        "JUnit report:\n{junit}"
    );
}

fn html_data(path: &Path) -> serde_json::Value {
    let html = fs::read_to_string(path).expect("HTML report should exist");
    let marker = "id=\"vault-report-data\"";
    let start = html.find(marker).expect("HTML embeds report data");
    let content = &html[start..];
    let content = &content[content.find('>').unwrap() + 1..];
    let content = &content[..content.find("</script>").unwrap()];
    serde_json::from_str(content).expect("embedded data should be JSON")
}

fn html_files(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory).unwrap() {
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
    files.sort();
    files
}

fn assert_offline_html_links(directory: &Path) {
    let stylesheet = directory.join("vault-report-pages/style.css");
    assert!(stylesheet.is_file(), "shared stylesheet should exist");
    assert!(
        !fs::read_to_string(&stylesheet).unwrap().is_empty(),
        "shared stylesheet should contain CSS"
    );
    for path in html_files(directory) {
        let html = fs::read_to_string(&path).unwrap();
        assert!(
            !html.contains("<script src="),
            "external script in {path:?}"
        );
        if html.contains("id=\"vault-report-data\"")
            || html.contains("<!-- vault-report-index:v1 -->")
        {
            assert!(!html.contains("<style"), "inline stylesheet in {path:?}");
            assert!(!html.contains(" style="), "inline style in {path:?}");
            assert_eq!(
                html.matches("<link rel=\"stylesheet\"").count(),
                1,
                "expected one stylesheet reference in {path:?}"
            );
            let href = if path.parent() == Some(directory) {
                "vault-report-pages/style.css"
            } else {
                "style.css"
            };
            assert!(
                html.contains(&format!("<link rel=\"stylesheet\" href=\"{href}\"")),
                "wrong stylesheet reference in {path:?}"
            );
        }
        for link in html.split("href=\"").skip(1) {
            let href = link.split('"').next().unwrap();
            if href.starts_with('#') {
                continue;
            }
            assert!(!href.contains(":"), "nonrelative link {href} in {path:?}");
            let encoded = href.split('#').next().unwrap().as_bytes();
            let mut decoded = Vec::new();
            let mut position = 0;
            while position < encoded.len() {
                if encoded[position] == b'%' {
                    decoded.push(
                        u8::from_str_radix(
                            std::str::from_utf8(&encoded[position + 1..position + 3]).unwrap(),
                            16,
                        )
                        .unwrap(),
                    );
                    position += 3;
                } else {
                    decoded.push(encoded[position]);
                    position += 1;
                }
            }
            let target = path
                .parent()
                .unwrap()
                .join(String::from_utf8(decoded).unwrap());
            assert!(target.is_file(), "broken link {href} in {path:?}");
        }
    }
}

fn write_preflight_failure_suite(root: &Path) {
    fs::create_dir_all(root).expect("preflight fixture directory should be created");
    fs::write(
        root.join("vault.yaml"),
        r#"version: 1
environments:
  local:
    target:
      base_url: http://127.0.0.1:1
      health_check: { path: /healthz, timeout: 1ms }
    mock_server: { bind: 127.0.0.1:0 }
"#,
    )
    .expect("preflight fixture config should be written");
    fs::write(
        root.join("preflight.test.yaml"),
        r#"test: preflight color sentinel
steps:
  - name: unreachable
    request: { method: GET, path: /never }
    expect: { status: 200 }
"#,
    )
    .expect("preflight fixture test should be written");
}

#[test]
fn suite_argument_accepts_a_directory_or_its_vault_yaml() {
    let suite = fixture("reporting");
    let config = suite.join("vault.yaml");

    for (label, suite_path) in [
        ("suite-dir", suite.as_path()),
        ("suite-file", config.as_path()),
    ] {
        let sandbox = TempDir::new(label);
        let mut command = vault_command(&sandbox);
        command.arg("list").arg("--suite-dir").arg(suite_path);
        let result = output(&mut command);
        assert_exit(&result, 0);
        assert!(
            text(&result.stdout).contains("2 standalone tests"),
            "stdout:\n{}",
            text(&result.stdout)
        );
    }
}

#[test]
fn missing_suite_path_is_a_config_error() {
    let sandbox = TempDir::new("missing-suite");
    let missing = sandbox.path().join("does-not-exist");
    let mut command = vault_command(&sandbox);
    command.arg("list").arg("--suite-dir").arg(&missing);

    let result = output(&mut command);
    assert_exit(&result, 2);
    let stderr = text(&result.stderr);
    assert!(stderr.contains("path does not exist"), "stderr:\n{stderr}");
    assert!(
        stderr.contains(&missing.display().to_string()),
        "stderr:\n{stderr}"
    );
}

#[test]
fn run_rejects_a_store_used_but_not_configured() {
    let sandbox = TempDir::new("missing-store");
    let mut command = vault_command(&sandbox);
    command
        .arg("run")
        .arg("uses missing store")
        .arg("--suite-dir")
        .arg(fixture("unconfigured-store"));

    let result = output(&mut command);
    assert_exit(&result, 2);
    let stderr = text(&result.stderr);
    assert!(
        stderr.contains("store `postgres` is used but not configured"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn quick_run_shorthand_preserves_pattern_and_options() {
    let sandbox = TempDir::new("quick-run");
    let server = HttpServer::start();
    let mut command = vault_command(&sandbox);
    command
        .arg("--run")
        .arg(fixture("reporting"))
        .arg("report pass")
        .arg("--tag")
        .arg("smoke")
        .env(TARGET_URL_ENV, server.url());

    let result = output(&mut command);
    assert_exit(&result, 0);
    assert_config_reports(&sandbox, "PASSED", 0);
}

#[test]
fn empty_run_fails_before_setup_but_empty_list_succeeds() {
    let suite = fixture("empty-selection");

    let run_sandbox = TempDir::new("empty-run");
    let json_path = run_sandbox.path().join("should-not-exist.json");
    let junit_path = run_sandbox.path().join("should-not-exist.xml");
    let html_path = run_sandbox.path().join("should-not-exist.html");
    let mut run = vault_command(&run_sandbox);
    run.arg("run")
        .arg(MISSING_PATTERN)
        .arg("--tag")
        .arg("smoke")
        .arg("--tag")
        .arg("regression")
        .arg("--suite-dir")
        .arg(&suite)
        .arg("--report")
        .arg(&json_path)
        .arg("--junit")
        .arg(&junit_path)
        .arg("--html")
        .arg(&html_path);
    let run_result = output(&mut run);
    assert_exit(&run_result, 2);

    let stderr = text(&run_result.stderr);
    assert!(stderr.contains(MISSING_PATTERN), "stderr:\n{stderr}");
    assert!(stderr.contains("smoke"), "stderr:\n{stderr}");
    assert!(stderr.contains("regression"), "stderr:\n{stderr}");
    assert!(
        !stderr.contains("mock listener bind failed"),
        "empty selection reached mock setup:\n{stderr}"
    );
    assert!(!json_path.exists());
    assert!(!junit_path.exists());
    assert!(!html_path.exists());

    let list_sandbox = TempDir::new("empty-list");
    let mut list = vault_command(&list_sandbox);
    list.arg("list")
        .arg(MISSING_PATTERN)
        .arg("--tag")
        .arg("smoke")
        .arg("--tag")
        .arg("regression")
        .arg("--suite-dir")
        .arg(&suite);
    let list_result = output(&mut list);
    assert_exit(&list_result, 0);
    assert!(
        text(&list_result.stdout).contains("0 flows, 0 standalone tests"),
        "stdout:\n{}",
        text(&list_result.stdout)
    );
}

#[test]
fn tag_only_empty_run_reports_the_absent_pattern() {
    let sandbox = TempDir::new("tag-only-empty-run");
    let mut command = vault_command(&sandbox);
    command
        .arg("run")
        .arg("--tag")
        .arg("missing-tag")
        .arg("--suite-dir")
        .arg(fixture("empty-selection"));

    let result = output(&mut command);
    assert_exit(&result, 2);
    let stderr = text(&result.stderr);
    assert!(stderr.contains("pattern: <none>"), "stderr:\n{stderr}");
    assert!(stderr.contains("tags: missing-tag"), "stderr:\n{stderr}");
    assert!(
        !stderr.contains("mock listener bind failed"),
        "empty selection reached mock setup:\n{stderr}"
    );
}

#[test]
fn matching_run_reaches_runtime_setup() {
    let sandbox = TempDir::new("matching-selection");
    let mut command = vault_command(&sandbox);
    command
        .arg("run")
        .arg(MATCHING_PATTERN)
        .arg("--tag")
        .arg("smoke")
        .arg("--tag")
        .arg("regression")
        .arg("--suite-dir")
        .arg(fixture("empty-selection"));

    let result = output(&mut command);
    assert_exit(&result, 3);
    let stderr = text(&result.stderr);
    assert!(
        stderr.contains("mock listener bind failed"),
        "matching selection did not reach mock setup:\n{stderr}"
    );
}

#[test]
fn passing_run_writes_reports_from_config() {
    let sandbox = TempDir::new("config-reports");
    let server = HttpServer::start();
    let mut command = configured_run(&sandbox, &server, "report pass");

    let result = output(&mut command);
    assert_exit(&result, 0);
    assert_config_reports(&sandbox, "PASSED", 0);
    let data = html_data(&sandbox.path().join("config-report.html"));
    assert_eq!(data["run"]["tests"][0]["status"], "PASSED");
    assert_eq!(data["run"]["schema_version"], 1);
    assert!(!data["executions"][0]["events"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn html_flag_overrides_config_and_preserves_selection_metadata() {
    let sandbox = TempDir::new("html-override");
    let server = HttpServer::start();
    let path = sandbox.path().join("nested/run.html");
    let mut command = configured_run(&sandbox, &server, "report pass");
    command
        .arg("--html")
        .arg(&path)
        .arg("--shuffle")
        .arg("--seed")
        .arg("42");
    assert_exit(&output(&mut command), 0);
    assert!(!sandbox.path().join("config-report.html").exists());
    let data = html_data(&path);
    assert_eq!(data["metadata"]["pattern"], "report pass");
    assert_eq!(data["metadata"]["shuffle_seed"], 42);
    assert_eq!(data["metadata"]["tests"][0]["source"], "pass.test.yaml");
    assert!(data["metadata"]["started_at"]
        .as_str()
        .unwrap()
        .contains('T'));
    assert!(data["executions"].to_string().contains("/pass"));
}

#[test]
fn html_bundle_lists_current_five_flows_and_standalone_and_replaces_reruns() {
    let sandbox = TempDir::new("html-flow-index");
    let server = HttpServer::start();
    let suite = sandbox.path().join("suite");
    let reports = sandbox.path().join("reports");
    fs::create_dir_all(&suite).unwrap();
    fs::copy(
        fixture("reporting").join("vault.yaml"),
        suite.join("vault.yaml"),
    )
    .unwrap();
    let shared_test = r#"test: shared stage
vars: { flow_key: default }
steps:
  - name: request for this flow
    request:
      method: GET
      path: /pass
      query: { flow: "{{ vars.flow_key }}" }
    expect: { status: 200 }
"#;
    fs::write(suite.join("shared.test.yaml"), shared_test).unwrap();
    fs::write(
        suite.join("shared-next.test.yaml"),
        shared_test.replace("test: shared stage", "test: shared next stage"),
    )
    .unwrap();
    fs::write(
        suite.join("standalone.test.yaml"),
        "test: standalone check\nsteps:\n  - name: standalone request\n    request: { method: GET, path: /pass }\n    expect: { status: 200 }\n",
    ).unwrap();
    let flows = ["merchant", "shop", "provider", "zone", "rule"];
    for (index, name) in flows.iter().enumerate() {
        fs::write(
            suite.join(format!("indexed-{index}.flow.yaml")),
            format!(
                "flow: {name} CRUD\non_failure: skip-rest\nstages:\n  - test: shared stage\n    with: {{ flow_key: {name}-first }}\n  - test: shared next stage\n    with: {{ flow_key: {name}-second }}\n"
            ),
        ).unwrap();
    }
    let mut command = vault_command(&sandbox);
    command
        .arg("run")
        .arg("--suite-dir")
        .arg(&suite)
        .arg("--html")
        .arg(reports.join("suite #report.html"))
        .arg("--report")
        .arg(reports.join("report.json"))
        .arg("--junit")
        .arg(reports.join("suite-junit.xml"))
        .env(TARGET_URL_ENV, server.url());
    assert_exit(&output(&mut command), 0);

    let aggregate = html_data(&reports.join("suite #report.html"));
    let items = aggregate["metadata"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 6);
    assert_eq!(aggregate["run"]["tests"].as_array().unwrap().len(), 11);
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(reports.join("report.json")).unwrap()).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["tests"].as_array().unwrap().len(), 11);
    assert!(json.get("executions").is_none());
    let junit = fs::read_to_string(reports.join("suite-junit.xml")).unwrap();
    assert!(junit.contains(r#"tests="11" failures="0" errors="0" skipped="0""#));
    assert_eq!(junit.matches("<testcase ").count(), 11);

    let index = fs::read_to_string(reports.join("index.html")).unwrap();
    let mut previous_position = 0;
    for (position, name) in flows.iter().enumerate() {
        assert_eq!(items[position]["name"], format!("{name} CRUD"));
        let row_position = index.find(&format!("{name} CRUD")).unwrap();
        assert!(
            row_position > previous_position,
            "index follows execution order"
        );
        previous_position = row_position;
        let filename = format!("vault-report-pages/flow-{:03}.html", position + 1);
        assert_eq!(index.matches(&format!(r#"href="{filename}""#)).count(), 1);
        let detail = html_data(&reports.join(&filename));
        assert_eq!(detail["metadata"]["items"].as_array().unwrap().len(), 1);
        assert_eq!(
            detail["metadata"]["items"][0]["name"],
            format!("{name} CRUD")
        );
        assert_eq!(detail["run"]["tests"].as_array().unwrap().len(), 2);
        assert_eq!(detail["executions"].as_array().unwrap().len(), 2);
        let descriptors = detail["metadata"]["tests"].as_array().unwrap();
        assert_eq!(descriptors.len(), 2);
        assert_ne!(descriptors[0]["id"], descriptors[1]["id"]);
        for (stage, descriptor) in descriptors.iter().enumerate() {
            assert_eq!(
                descriptor["name"],
                ["shared stage", "shared next stage"][stage]
            );
            assert_eq!(descriptor["stage_index"], stage);
            assert_eq!(descriptor["result_index"], stage);
            assert_eq!(descriptor["execution_index"], stage);
        }
        let evidence = detail["executions"].to_string();
        assert!(evidence.contains(&format!("{name}-first")));
        assert!(evidence.contains(&format!("{name}-second")));
        for other in flows.iter().filter(|other| *other != name) {
            assert!(!evidence.contains(&format!("{other}-first")));
        }
        assert!(fs::read_to_string(reports.join(filename))
            .unwrap()
            .contains(r#"href="../index.html""#));
    }
    assert!(index.contains("standalone check"));
    assert!(index.contains(r#"data-status="PASSED""#));
    let standalone = html_data(&reports.join("vault-report-pages/test-006.html"));
    assert_eq!(standalone["metadata"]["items"][0]["kind"], "test");
    assert_eq!(standalone["metadata"]["tests"][0]["result_index"], 0);
    assert_eq!(standalone["metadata"]["tests"][0]["execution_index"], 0);
    assert_eq!(standalone["run"]["tests"].as_array().unwrap().len(), 1);
    assert!(!reports.join(".vault-report-index.json").exists());
    assert_offline_html_links(&reports);

    // A rerun represents only its own selection and outcome. The next shared
    // stage remains visible when the first stage fails and skips it.
    fs::write(
        suite.join("shared.test.yaml"),
        shared_test.replace("status: 200", "status: 201"),
    )
    .unwrap();
    fs::write(reports.join("vault-report-pages/notes.html"), "user notes").unwrap();
    let mut rerun = vault_command(&sandbox);
    rerun
        .arg("run")
        .arg("merchant CRUD")
        .arg("--suite-dir")
        .arg(&suite)
        .arg("--html")
        .arg(reports.join("suite #report.html"))
        .arg("--report")
        .arg(reports.join("report.json"))
        .arg("--junit")
        .arg(reports.join("suite-junit.xml"))
        .env(TARGET_URL_ENV, server.url());
    for _ in 0..2 {
        assert_exit(&output(&mut rerun), 1);
    }
    let index = fs::read_to_string(reports.join("index.html")).unwrap();
    assert_eq!(
        index
            .matches(r#"href="vault-report-pages/flow-001.html""#)
            .count(),
        1
    );
    assert!(index.contains("merchant CRUD"));
    assert!(index.contains(r#"data-status="FAILED""#));
    for name in flows.iter().skip(1) {
        assert!(!index.contains(&format!("{name} CRUD")));
    }
    assert!(!index.contains("standalone check"));
    let details = reports.join("vault-report-pages");
    for ordinal in 2..=5 {
        assert!(!details.join(format!("flow-{ordinal:03}.html")).exists());
    }
    assert!(!details.join("test-006.html").exists());
    assert_eq!(
        fs::read_to_string(details.join("notes.html")).unwrap(),
        "user notes"
    );
    let failed = html_data(&details.join("flow-001.html"));
    assert_eq!(failed["run"]["tests"][0]["status"], "FAILED");
    assert_eq!(failed["run"]["tests"][1]["status"], "SKIPPED");
    assert!(failed["executions"][1].to_string().contains("NOT_RUN"));
    assert_eq!(
        html_data(&reports.join("suite #report.html"))["metadata"]["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let moved = sandbox.path().join("downloaded-reports");
    fs::rename(&reports, &moved).unwrap();
    assert_offline_html_links(&moved);
}

#[test]
fn html_index_path_preserves_aggregate_and_flow_links() {
    let sandbox = TempDir::new("html-index-as-aggregate");
    let server = HttpServer::start();
    let path = sandbox.path().join("reports/index.html");
    let mut command = vault_command(&sandbox);
    command
        .arg("run")
        .arg("repeated report*")
        .arg("--suite-dir")
        .arg(fixture("html-reporting"))
        .arg("--html")
        .arg(&path)
        .env(TARGET_URL_ENV, server.url());
    for _ in 0..2 {
        assert_exit(&output(&mut command), 0);
    }
    let aggregate = html_data(&path);
    assert_eq!(aggregate["metadata"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(aggregate["run"]["tests"].as_array().unwrap().len(), 3);
    let html = fs::read_to_string(&path).unwrap();
    assert_eq!(
        html.matches(r#"href="vault-report-pages/flow-001.html""#)
            .count(),
        1
    );
    assert_eq!(
        html.matches(r#"href="vault-report-pages/flow-002.html""#)
            .count(),
        1
    );
    assert!(!sandbox.path().join("result.html").exists());
    assert_offline_html_links(path.parent().unwrap());
}

#[test]
fn html_bundle_write_failure_preserves_independent_exports() {
    let server = HttpServer::start();
    for blocker in ["index.html", "vault-report-pages"] {
        let sandbox = TempDir::new("html-bundle-error");
        let blocked = sandbox.path().join(blocker);
        if blocker == "index.html" {
            fs::create_dir(&blocked).unwrap();
        } else {
            fs::write(&blocked, "unrelated file").unwrap();
        }
        let mut command = configured_run(&sandbox, &server, "report pass");
        let result = output(&mut command);
        assert_exit(&result, 3);
        assert_config_reports(&sandbox, "PASSED", 0);
        let stderr = text(&result.stderr);
        assert!(
            stderr.contains("could not write HTML report"),
            "stderr: {stderr}"
        );
        assert!(stderr.contains(blocker), "stderr: {stderr}");
        if blocker == "vault-report-pages" {
            assert!(!sandbox.path().join("index.html").exists());
            assert_eq!(fs::read_to_string(blocked).unwrap(), "unrelated file");
        }
    }
}

#[test]
fn html_stylesheet_write_failure_preserves_independent_exports() {
    let sandbox = TempDir::new("html-stylesheet-error");
    let server = HttpServer::start();
    let mut command = configured_run(&sandbox, &server, "report pass");
    assert_exit(&output(&mut command), 0);
    let index = fs::read_to_string(sandbox.path().join("index.html")).unwrap();
    let stylesheet = sandbox.path().join("vault-report-pages/style.css");
    fs::remove_file(&stylesheet).unwrap();
    fs::create_dir(&stylesheet).unwrap();

    let result = output(&mut command);
    assert_exit(&result, 3);
    assert_config_reports(&sandbox, "PASSED", 0);
    let stderr = text(&result.stderr);
    assert!(stderr.contains("could not write HTML report"), "{stderr}");
    assert!(stderr.contains("style.css"), "{stderr}");
    assert!(stylesheet.is_dir(), "the blocking directory is preserved");
    assert_eq!(
        fs::read_to_string(sandbox.path().join("index.html")).unwrap(),
        index
    );
}

#[test]
fn html_json_and_junit_failures_are_independent() {
    let server = HttpServer::start();
    for (format, flag, filename) in [
        ("HTML", "--html", "config-report.html"),
        ("JSON", "--report", "config-report.json"),
        ("JUnit", "--junit", "config-junit.xml"),
    ] {
        let sandbox = TempDir::new("independent-reports");
        let blocker = blocked_report_path(&sandbox, "blocker", filename);
        let mut command = configured_run(&sandbox, &server, "report pass");
        command.arg(flag).arg(&blocker);
        let result = output(&mut command);
        assert_exit(&result, 3);
        assert!(
            text(&result.stderr).contains(&format!("could not write {format} report to")),
            "stderr: {}",
            text(&result.stderr)
        );
        for sibling in [
            "config-report.html",
            "config-report.json",
            "config-junit.xml",
        ] {
            if sibling != filename {
                assert!(
                    sandbox.path().join(sibling).exists(),
                    "{sibling} should survive {format} failure"
                );
            }
        }
    }
    let sandbox = TempDir::new("all-report-errors");
    let mut command = configured_run(&sandbox, &server, "report pass");
    for (flag, filename) in [
        ("--html", "report.html"),
        ("--report", "report.json"),
        ("--junit", "junit.xml"),
    ] {
        command
            .arg(flag)
            .arg(blocked_report_path(&sandbox, filename, filename));
    }
    let result = output(&mut command);
    assert_exit(&result, 3);
    for format in ["HTML", "JSON", "JUnit"] {
        assert!(text(&result.stderr).contains(&format!("could not write {format} report to")));
    }
}

#[test]
fn html_flow_identity_and_masking_preserve_raw_matching() {
    let server = HttpServer::start();
    for (pattern, code) in [("repeated report*", 0), ("broken report stages", 1)] {
        let sandbox = TempDir::new("masked-flow");
        let mut command = vault_command(&sandbox);
        command
            .arg("run")
            .arg(pattern)
            .arg("--suite-dir")
            .arg(fixture("html-reporting"))
            .arg("--verbose")
            .env(TARGET_URL_ENV, server.url());
        let output = output(&mut command);
        assert_exit(&output, code);
        let mut surfaces = vec![text(&output.stdout), text(&output.stderr)];
        for filename in ["result.html", "result.json", "result.xml", "index.html"] {
            surfaces.push(fs::read_to_string(sandbox.path().join(filename)).unwrap());
        }
        for path in html_files(&sandbox.path().join("vault-report-pages")) {
            surfaces.push(fs::read_to_string(path).unwrap());
        }
        for secret in [
            "cli-secret-sentinel",
            "cli-custom-sentinel",
            "cli-query-sentinel",
            "cli-nested-sentinel",
            "cli-expected-sentinel",
            "cli-pattern-hidden",
        ] {
            assert!(
                surfaces.iter().all(|surface| !surface.contains(secret)),
                "secret {secret} leaked into report output"
            );
        }
        let data = html_data(&sandbox.path().join("result.html"));
        let descriptors = data["metadata"]["tests"].as_array().unwrap();
        assert_eq!(descriptors.len(), if code == 0 { 3 } else { 2 });
        assert_ne!(descriptors[0]["id"], descriptors[1]["id"]);
        assert_eq!(descriptors[0]["stage_index"], 0);
        if code == 0 {
            // Flow files are discovered in lexical order: the one-stage
            // "other" flow precedes the two-stage flow.
            assert_eq!(descriptors[1]["stage_index"], 0);
            assert_eq!(descriptors[2]["stage_index"], 1);
            assert_eq!(descriptors[0]["name"], descriptors[1]["name"]);
            assert!(data["run"]["tests"]
                .as_array()
                .unwrap()
                .iter()
                .all(|test| test["status"] == "PASSED"));
            assert!(data["executions"][0]["exports"]["access_token"]
                .as_str()
                .unwrap()
                .contains("REDACTED"));
        } else {
            assert_eq!(descriptors[1]["stage_index"], 1);
            assert_eq!(data["run"]["tests"][1]["status"], "SKIPPED");
            assert!(data["executions"].to_string().contains("NOT_RUN"));
        }
        let json: serde_json::Value = serde_json::from_str(&surfaces[3]).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert!(json.get("executions").is_none());
    }
}

#[test]
fn html_preserves_transport_error_and_unexecuted_steps() {
    let sandbox = TempDir::new("html-error");
    let server = HttpServer::start();
    let mut command = vault_command(&sandbox);
    command
        .arg("run")
        .arg("report transport error")
        .arg("--suite-dir")
        .arg(fixture("html-reporting"))
        .env(TARGET_URL_ENV, server.url());
    assert_exit(&output(&mut command), 3);
    let data = html_data(&sandbox.path().join("result.html"));
    assert_eq!(data["run"]["tests"][0]["status"], "ERRORED");
    let events = data["executions"][0]["events"].as_array().unwrap();
    assert!(events
        .iter()
        .any(|event| event["status"] == "ERRORED" && event["step_index"] == 0));
    assert!(events
        .iter()
        .any(|event| event["status"] == "NOT_RUN" && event["step_index"] == 1));
    let detail = html_data(&sandbox.path().join("vault-report-pages/test-001.html"));
    assert_eq!(detail["run"]["tests"], data["run"]["tests"]);
    assert_eq!(detail["executions"], data["executions"]);
    assert!(fs::read_to_string(sandbox.path().join("index.html"))
        .unwrap()
        .contains(r#"data-status="ERRORED""#));
}

#[test]
fn invalid_report_masking_is_a_configuration_error_before_execution() {
    let sandbox = TempDir::new("invalid-mask");
    let suite = sandbox.path().join("suite");
    write_preflight_failure_suite(&suite);
    let config_path = suite.join("vault.yaml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\nreport:\n  html: forbidden.html\n  redact:\n    text_patterns: ['[']\n");
    fs::write(config_path, config).unwrap();
    for operation in ["run", "validate"] {
        let mut command = vault_command(&sandbox);
        command.arg(operation).arg("--suite-dir").arg(&suite);
        let result = output(&mut command);
        assert_exit(&result, 2);
        assert!(text(&result.stderr).contains("report.redact"));
        assert!(!sandbox.path().join("forbidden.html").exists());
    }
}

#[test]
fn request_credentials_are_masked_in_report_echoes_without_html_collection() {
    let sandbox = TempDir::new("mask-without-html");
    let server = HttpServer::start();
    let suite = sandbox.path().join("suite");
    fs::create_dir_all(&suite).unwrap();
    let config = fs::read_to_string(fixture("html-reporting").join("vault.yaml"))
        .unwrap()
        .replace("  html: result.html\n", "");
    fs::write(suite.join("vault.yaml"), config).unwrap();
    fs::copy(
        fixture("html-reporting").join("pass.test.yaml"),
        suite.join("pass.test.yaml"),
    )
    .unwrap();
    let mut command = vault_command(&sandbox);
    command
        .arg("run")
        .arg("--suite-dir")
        .arg(&suite)
        .arg("--verbose")
        .env(TARGET_URL_ENV, server.url());
    let result = output(&mut command);
    assert_exit(&result, 0);
    assert!(!sandbox.path().join("result.html").exists());
    let json = fs::read_to_string(sandbox.path().join("result.json")).unwrap();
    for secret in ["cli-custom-sentinel", "cli-query-sentinel"] {
        assert!(
            !json.contains(secret),
            "request credential echo {secret} leaked without HTML"
        );
        assert!(!text(&result.stdout).contains(secret));
    }
}

#[test]
fn long_and_encoded_credential_echoes_are_masked_after_original_matching() {
    let server = HttpServer::start();
    let long = format!("cli-long-raw-{}", "abcdef0123456789".repeat(300));
    let known = "cli-encoded:a/b?c=d&e+f";
    let encoded = encode_test_component(known);
    let raw_body =
        format!("{long}\nhttp://example.invalid/echo?public_echo={encoded}\npublic_echo={encoded}");
    // Exercise both the private source-only path and full HTML evidence.
    for collect_html in [false, true] {
        let sandbox = TempDir::new("long-encoded-credentials");
        let suite = sandbox.path().join("suite");
        fs::create_dir_all(&suite).unwrap();
        let mut config = serde_json::json!({
            "version": 1,
            "environments": {"local": {
                "target": {"base_url": server.url()},
                "mock_server": {"bind": "127.0.0.1:0"}
            }},
            "report": {
                "json": "result.json", "junit": "result.xml",
                "redact": {"headers": ["X-Long-Token", "X-Encoded-Credential"]}
            }
        });
        if collect_html {
            config["report"]["html"] = serde_json::json!("result.html");
        }
        fs::write(
            suite.join("vault.yaml"),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .unwrap();
        let request = serde_json::json!({
            "method": "GET", "path": "/credential-echo",
            "headers": {"X-Long-Token": long, "X-Encoded-Credential": known}
        });
        for (filename, name, expected_body) in [
            (
                "01-pass.test.yaml",
                "original credential matching".to_owned(),
                raw_body.clone(),
            ),
            (
                "02-fail.test.yaml",
                format!("encoded credential echo {encoded}"),
                "deliberate body mismatch".to_owned(),
            ),
        ] {
            let test = serde_json::json!({
                "test": name,
                "steps": [{"name": "echo", "request": request, "expect": {
                    "status": 200, "headers": {"X-Proof": long}, "body": expected_body
                }}]
            });
            fs::write(
                suite.join(filename),
                serde_json::to_vec_pretty(&test).unwrap(),
            )
            .unwrap();
        }
        let mut command = vault_command(&sandbox);
        command
            .arg("run")
            .arg("--suite-dir")
            .arg(&suite)
            .arg("--verbose")
            .arg("--no-color");
        let result = output(&mut command);
        assert_exit(&result, 1);
        let mut surfaces = vec![
            ("stdout", text(&result.stdout)),
            ("stderr", text(&result.stderr)),
        ];
        for filename in ["result.json", "result.xml"] {
            surfaces.push((
                filename,
                fs::read_to_string(sandbox.path().join(filename)).unwrap(),
            ));
        }
        if collect_html {
            surfaces.push((
                "result.html",
                fs::read_to_string(sandbox.path().join("result.html")).unwrap(),
            ));
        } else {
            assert!(!sandbox.path().join("result.html").exists());
        }
        // The 500-byte body diff and 2000-byte response preview must not expose
        // even the beginning of a known credential after legacy truncation.
        for (surface, contents) in &surfaces {
            for (kind, secret) in [
                ("long prefix", &long[..80]),
                ("raw credential", known),
                ("encoded credential", encoded.as_str()),
            ] {
                assert!(
                    !contents.contains(secret),
                    "{kind} leaked in {surface} with HTML={collect_html}"
                );
            }
        }
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(sandbox.path().join("result.json")).unwrap())
                .unwrap();
        let tests = json["tests"].as_array().unwrap();
        assert_eq!(
            tests[0]["status"], "PASSED",
            "original raw body and proof header must still match"
        );
        assert_eq!(tests[1]["status"], "FAILED");
        let checks = tests[1]["steps"][0]["checks"]["checks"].as_array().unwrap();
        assert_eq!(
            checks
                .iter()
                .filter(|check| check["result"] == "fail")
                .count(),
            1
        );
        assert_eq!(
            checks.last().unwrap()["yaml_path"],
            "steps.echo.expect.body"
        );
        assert!(
            surfaces[0].1.contains("REDACTED"),
            "the failing terminal body diff should contain a masked preview"
        );
        assert!(surfaces[2].1.contains("REDACTED"));
        assert!(
            surfaces[3].1.contains("REDACTED"),
            "JUnit must mask the encoded test name too"
        );
    }
}

#[test]
fn json_and_junit_write_failures_are_both_reported() {
    let sandbox = TempDir::new("two-report-errors");
    let server = HttpServer::start();
    let json_path = blocked_report_path(&sandbox, "json-blocker", "report.json");
    let junit_path = blocked_report_path(&sandbox, "junit-blocker", "report.xml");
    let mut command = configured_run(&sandbox, &server, "report pass");
    command
        .arg("--report")
        .arg(&json_path)
        .arg("--junit")
        .arg(&junit_path);

    let result = output(&mut command);
    assert_exit(&result, 3);
    let stderr = text(&result.stderr);
    assert!(
        stderr.contains("could not write JSON report to"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("could not write JUnit report to"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&json_path.display().to_string()),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&junit_path.display().to_string()),
        "stderr:\n{stderr}"
    );
}

#[test]
fn failed_json_write_does_not_prevent_junit_write() {
    let sandbox = TempDir::new("report-write-order");
    let server = HttpServer::start();
    let json_path = blocked_report_path(&sandbox, "json-blocker", "report.json");
    let junit_path = sandbox.path().join("sibling-report.xml");
    let mut command = configured_run(&sandbox, &server, "report pass");
    command
        .arg("--report")
        .arg(&json_path)
        .arg("--junit")
        .arg(&junit_path);

    let result = output(&mut command);
    assert_exit(&result, 3);
    assert!(fs::read_to_string(&junit_path)
        .expect("JUnit sibling report should be written")
        .contains("<testsuite"));
}

#[test]
fn failed_junit_write_does_not_prevent_json_write() {
    let sandbox = TempDir::new("reverse-report-write-order");
    let server = HttpServer::start();
    let json_path = sandbox.path().join("sibling-report.json");
    let junit_path = blocked_report_path(&sandbox, "junit-blocker", "report.xml");
    let mut command = configured_run(&sandbox, &server, "report pass");
    command
        .arg("--report")
        .arg(&json_path)
        .arg("--junit")
        .arg(&junit_path);

    let result = output(&mut command);
    assert_exit(&result, 3);
    assert!(fs::read_to_string(&json_path)
        .expect("JSON sibling report should be written")
        .contains(r#""status": "PASSED""#));
}

#[test]
fn test_failure_with_successful_reports_preserves_exit_one() {
    let sandbox = TempDir::new("failed-test-reports");
    let server = HttpServer::start();
    let mut command = configured_run(&sandbox, &server, "report fail");

    let result = output(&mut command);
    assert_exit(&result, 1);
    assert_config_reports(&sandbox, "FAILED", 1);
}

#[test]
fn color_captured_default_output_is_plain() {
    let list_sandbox = TempDir::new("color-default-list");
    let mut list = vault_command(&list_sandbox);
    list.arg("list")
        .arg("--suite-dir")
        .arg(fixture("reporting"));

    let list_result = output(&mut list);
    assert_exit(&list_result, 0);
    assert_no_ansi_output(&list_result);

    let error_sandbox = TempDir::new("color-default-error");
    let missing = error_sandbox.path().join("does-not-exist");
    let mut error = vault_command(&error_sandbox);
    error.arg("list").arg("--suite-dir").arg(missing);

    let error_result = output(&mut error);
    assert_exit(&error_result, 2);
    assert_no_ansi_output(&error_result);
}

#[test]
fn color_force_is_a_positive_control_for_runtime_and_clap() {
    let list_sandbox = TempDir::new("color-force-list");
    let mut list = vault_command(&list_sandbox);
    list.arg("list")
        .arg("--suite-dir")
        .arg(fixture("reporting"))
        .env("CLICOLOR_FORCE", "1");

    let list_result = output(&mut list);
    assert_exit(&list_result, 0);
    assert_has_ansi("forced runtime stdout", &list_result.stdout);

    let help_sandbox = TempDir::new("color-force-help");
    let mut help = vault_command(&help_sandbox);
    help.arg("--help").env("CLICOLOR_FORCE", "1");

    let help_result = output(&mut help);
    assert_exit(&help_result, 0);
    assert_has_ansi("forced help stdout", &help_result.stdout);

    let error_sandbox = TempDir::new("color-force-clap-error");
    let mut error = vault_command(&error_sandbox);
    error
        .arg("definitely-not-a-command")
        .env("CLICOLOR_FORCE", "1");

    let error_result = output(&mut error);
    assert_exit(&error_result, 2);
    assert_has_ansi("forced parse-error stderr", &error_result.stderr);
}

#[test]
fn color_no_color_flag_wins_over_force_for_runtime_help_and_parse_errors() {
    for (label, flag_before_subcommand) in [
        ("color-flag-before-subcommand", true),
        ("color-flag-after-subcommand", false),
    ] {
        let sandbox = TempDir::new(label);
        let mut command = vault_command(&sandbox);
        if flag_before_subcommand {
            command.arg("--no-color");
        }
        command
            .arg("list")
            .arg("--suite-dir")
            .arg(fixture("reporting"));
        if !flag_before_subcommand {
            command.arg("--no-color");
        }
        command.env("CLICOLOR_FORCE", "1");

        let result = output(&mut command);
        assert_exit(&result, 0);
        assert_no_ansi_output(&result);
    }

    let help_sandbox = TempDir::new("color-flag-help");
    let mut help = vault_command(&help_sandbox);
    help.arg("--no-color")
        .arg("--help")
        .env("CLICOLOR_FORCE", "1");

    let help_result = output(&mut help);
    assert_exit(&help_result, 0);
    assert_no_ansi_output(&help_result);
    assert!(
        text(&help_result.stdout).contains("Disable ANSI colors"),
        "stdout:\n{}",
        text(&help_result.stdout)
    );

    let error_sandbox = TempDir::new("color-flag-clap-error");
    let mut error = vault_command(&error_sandbox);
    error
        .arg("--no-color")
        .arg("definitely-not-a-command")
        .env("CLICOLOR_FORCE", "1");

    let error_result = output(&mut error);
    assert_exit(&error_result, 2);
    assert_no_ansi_output(&error_result);
    assert!(
        text(&error_result.stderr).contains("unrecognized subcommand"),
        "stderr:\n{}",
        text(&error_result.stderr)
    );
}

#[test]
fn color_no_color_environment_wins_over_force_but_empty_value_is_unset() {
    let list_sandbox = TempDir::new("color-env-list");
    let mut list = vault_command(&list_sandbox);
    list.arg("list")
        .arg("--suite-dir")
        .arg(fixture("reporting"))
        .env("NO_COLOR", "1")
        .env("CLICOLOR_FORCE", "1");

    let list_result = output(&mut list);
    assert_exit(&list_result, 0);
    assert_no_ansi_output(&list_result);

    let error_sandbox = TempDir::new("color-env-clap-error");
    let mut error = vault_command(&error_sandbox);
    error
        .arg("definitely-not-a-command")
        .env("NO_COLOR", "1")
        .env("CLICOLOR_FORCE", "1");

    let error_result = output(&mut error);
    assert_exit(&error_result, 2);
    assert_no_ansi_output(&error_result);

    let clicolor_sandbox = TempDir::new("color-clicolor-zero");
    let mut clicolor = vault_command(&clicolor_sandbox);
    clicolor
        .arg("list")
        .arg("--suite-dir")
        .arg(fixture("reporting"))
        .env("CLICOLOR", "0")
        .env("CLICOLOR_FORCE", "1");

    let clicolor_result = output(&mut clicolor);
    assert_exit(&clicolor_result, 0);
    assert_no_ansi_output(&clicolor_result);

    let empty_list_sandbox = TempDir::new("color-empty-env-list");
    let mut empty_list = vault_command(&empty_list_sandbox);
    empty_list
        .arg("list")
        .arg("--suite-dir")
        .arg(fixture("reporting"))
        .env("NO_COLOR", "")
        .env("CLICOLOR_FORCE", "1");

    let empty_list_result = output(&mut empty_list);
    assert_exit(&empty_list_result, 0);
    assert_has_ansi(
        "runtime stdout with empty NO_COLOR",
        &empty_list_result.stdout,
    );

    let empty_help_sandbox = TempDir::new("color-empty-env-help");
    let mut empty_help = vault_command(&empty_help_sandbox);
    empty_help
        .arg("--help")
        .env("NO_COLOR", "")
        .env("CLICOLOR_FORCE", "1");

    let empty_help_result = output(&mut empty_help);
    assert_exit(&empty_help_result, 0);
    assert_has_ansi("help stdout with empty NO_COLOR", &empty_help_result.stdout);
}

#[test]
fn color_no_color_works_with_quick_run_before_or_after_shorthand() {
    let suite = fixture("empty-selection");
    for (label, flag_before_shorthand) in [
        ("color-quick-run-leading", true),
        ("color-quick-run-trailing", false),
    ] {
        let sandbox = TempDir::new(label);
        let mut command = vault_command(&sandbox);
        if flag_before_shorthand {
            command.arg("--no-color");
        }
        command
            .arg("--run")
            .arg(&suite)
            .arg(MISSING_PATTERN)
            .arg("--tag")
            .arg("smoke");
        if !flag_before_shorthand {
            command.arg("--no-color");
        }
        command.env("CLICOLOR_FORCE", "1");

        let result = output(&mut command);
        assert_exit(&result, 2);
        assert_no_ansi_output(&result);
        let stderr = text(&result.stderr);
        assert!(stderr.contains(MISSING_PATTERN), "stderr:\n{stderr}");
        assert!(
            !stderr.contains("mock listener bind failed"),
            "empty selection reached mock setup:\n{stderr}"
        );
    }
}

fn assert_pretty_output_without_color(
    label: &str,
    pattern: &str,
    expected_exit: i32,
    expected_summary: &str,
    expected_status: &str,
    expected_failures: usize,
) {
    let sandbox = TempDir::new(label);
    let server = HttpServer::start();
    let mut command = configured_run(&sandbox, &server, pattern);
    command.arg("--no-color").env("CLICOLOR_FORCE", "1");

    let result = output(&mut command);
    assert_exit(&result, expected_exit);
    assert_no_ansi_output(&result);
    assert!(
        text(&result.stdout).contains(expected_summary),
        "stdout:\n{}",
        text(&result.stdout)
    );
    assert_config_reports(&sandbox, expected_status, expected_failures);
}

#[test]
fn color_no_color_covers_successful_pretty_reports() {
    assert_pretty_output_without_color(
        "color-pretty-pass",
        "report pass",
        0,
        "1 passed · 0 failed",
        "PASSED",
        0,
    );
}

#[test]
fn color_no_color_covers_failed_pretty_reports() {
    assert_pretty_output_without_color(
        "color-pretty-fail",
        "report fail",
        1,
        "0 passed · 1 failed",
        "FAILED",
        1,
    );
}

#[test]
fn color_no_color_covers_preflight_diagnostics() {
    let preflight_sandbox = TempDir::new("color-preflight");
    let suite = preflight_sandbox.path().join("suite");
    write_preflight_failure_suite(&suite);
    let mut preflight = vault_command(&preflight_sandbox);
    preflight
        .arg("run")
        .arg("preflight color sentinel")
        .arg("--suite-dir")
        .arg(&suite)
        .arg("--no-color")
        .env("CLICOLOR_FORCE", "1");

    let preflight_result = output(&mut preflight);
    assert_exit(&preflight_result, 3);
    assert_no_ansi_output(&preflight_result);
    let stderr = text(&preflight_result.stderr);
    assert!(stderr.contains("preflight failed:"), "stderr:\n{stderr}");
    assert!(
        stderr.contains("target http://127.0.0.1:1"),
        "stderr:\n{stderr}"
    );
}
