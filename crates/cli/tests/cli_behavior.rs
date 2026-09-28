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
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let request = String::from_utf8_lossy(&request);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let (status, body) = match path {
        "/pass" | "/healthz" => ("200 OK", r#"{"status":"ok"}"#),
        _ => ("404 Not Found", r#"{"error":"not_found"}"#),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
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
    command.current_dir(sandbox.path());
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
        .arg(&junit_path);
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
