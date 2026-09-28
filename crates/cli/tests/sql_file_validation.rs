use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_DIR_ID: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let id = TEMP_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vault-cli-sql-file-{label}-{}-{id}",
            std::process::id()
        ));
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

fn write_suite(root: &Path, sql_file: &str, create_file: bool) {
    fs::create_dir_all(root.join("fixtures")).expect("SQL fixture directory should be created");
    fs::write(
        root.join("vault.yaml"),
        r#"version: 1
environments:
  local:
    target: { base_url: "http://127.0.0.1:9" }
    postgres: { url: "postgres://127.0.0.1:9/not-used" }
    mock_server: { bind: "127.0.0.1:0" }
"#,
    )
    .expect("SQL fixture suite config should be written");
    fs::write(
        root.join("fixture.test.yaml"),
        format!(
            r#"test: validate SQL file fixture
seed:
  postgres:
    - sql_file: "{sql_file}"
steps: []
"#
        ),
    )
    .expect("SQL fixture test definition should be written");
    if create_file {
        fs::write(root.join(sql_file), "SELECT 1;\n").expect("SQL fixture file should be written");
    }
}

fn run_validate(cwd: &Path, suite: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vault"))
        .current_dir(cwd)
        .arg("validate")
        .arg("--suite-dir")
        .arg(suite)
        .output()
        .expect("vault subprocess should start")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn validate_resolves_sql_files_before_external_service_io() {
    let workspace = TempDir::new("workspace");
    let command_cwd = TempDir::new("unrelated-cwd");

    let valid_suite = workspace.path().join("valid-suite");
    write_suite(&valid_suite, "fixtures/seed.sql", true);
    let valid = run_validate(command_cwd.path(), &valid_suite);
    assert_eq!(
        valid.status.code(),
        Some(0),
        "stderr:\n{}",
        text(&valid.stderr)
    );
    assert!(
        text(&valid.stdout).contains("no issues"),
        "stdout:\n{}",
        text(&valid.stdout)
    );

    let missing_suite = workspace.path().join("missing-suite");
    write_suite(&missing_suite, "fixtures/missing.sql", false);
    let missing = run_validate(command_cwd.path(), &missing_suite);
    assert_eq!(
        missing.status.code(),
        Some(2),
        "stdout:\n{}\nstderr:\n{}",
        text(&missing.stdout),
        text(&missing.stderr)
    );
    let stderr = text(&missing.stderr);
    assert!(stderr.contains("seed.postgres[0].sql_file"), "{stderr}");
    assert!(stderr.contains("fixtures/missing.sql"), "{stderr}");
    assert!(stderr.contains("could not resolve SQL file"), "{stderr}");
    assert!(
        !stderr.contains("connection error"),
        "validation unexpectedly contacted PostgreSQL:\n{stderr}"
    );
}
