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

fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).expect("fixture path should be representable as YAML")
}

fn yaml_path(path: &Path) -> String {
    yaml_string(&path.to_string_lossy().replace('\\', "/"))
}

fn write_vault(root: &Path, global_seed: &str) {
    fs::create_dir_all(root).expect("suite directory should be created");
    fs::write(
        root.join("vault.yaml"),
        format!(
            r#"version: 1
environments:
  local:
    target: {{ base_url: "http://127.0.0.1:9" }}
    postgres: {{ url: "postgres://127.0.0.1:9/not-used" }}
    mock_server: {{ bind: "127.0.0.1:0" }}
{global_seed}"#
        ),
    )
    .expect("suite config should be written");
}

fn write_test(path: &Path, name: &str, seed_entries: &str) {
    fs::create_dir_all(path.parent().expect("test path should have a parent"))
        .expect("test directory should be created");
    fs::write(
        path,
        format!(
            r#"test: {name}
seed:
  postgres:
{seed_entries}steps: []
"#
        ),
    )
    .expect("test definition should be written");
}

fn write_sql(path: &Path) {
    fs::create_dir_all(path.parent().expect("SQL path should have a parent"))
        .expect("SQL fixture directory should be created");
    fs::write(path, "SELECT 1;\n").expect("SQL fixture should be written");
}

fn vault_command(cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vault"));
    command
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");
    command
}

fn run_validate(cwd: &Path, suite: &Path) -> Output {
    vault_command(cwd)
        .arg("validate")
        .arg("--suite-dir")
        .arg(suite)
        .output()
        .expect("vault subprocess should start")
}

fn run_suite(cwd: &Path, suite: &Path) -> Output {
    vault_command(cwd)
        .arg("run")
        .arg("--suite-dir")
        .arg(suite)
        .output()
        .expect("vault subprocess should start")
}

fn run_list_with_fixtures(cwd: &Path, suite: &Path) -> Output {
    vault_command(cwd)
        .arg("list")
        .arg("--suite-dir")
        .arg(suite)
        .arg("--fixtures")
        .output()
        .expect("vault subprocess should start")
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
        text(&output.stderr)
    );
}

fn assert_in_order(haystack: &str, needles: &[&str]) {
    let mut position = 0;
    for needle in needles {
        let offset = haystack[position..]
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` missing after byte {position}:\n{haystack}"));
        position += offset + needle.len();
    }
}

#[test]
fn validate_resolves_parent_relative_file_from_declaring_yaml_and_unrelated_cwd() {
    let workspace = TempDir::new("parent-relative");
    let command_cwd = TempDir::new("unrelated-cwd");
    let suite = workspace.path().join("project/suite");
    let test_path = suite.join("cases/deep/parent-relative.test.yaml");

    write_vault(&suite, "");
    write_sql(&suite.join("fixtures/parent.sql"));
    write_test(
        &test_path,
        "parent relative fixture",
        "    - sql_file: ../../fixtures/parent.sql\n",
    );

    let output = run_validate(command_cwd.path(), &suite);
    assert_exit(&output, 0);
    assert!(
        text(&output.stdout).contains("no issues"),
        "stdout:\n{}",
        text(&output.stdout)
    );
}

#[test]
fn validate_accepts_absolute_file_and_glob_selectors() {
    let workspace = TempDir::new("absolute");
    let command_cwd = TempDir::new("absolute-cwd");
    let suite = workspace.path().join("suite");
    let external = workspace.path().join("external-fixtures");
    let exact = external.join("exact.sql");
    let glob_dir = external.join("glob");

    write_vault(&suite, "");
    write_sql(&exact);
    write_sql(&glob_dir.join("20-second.sql"));
    write_sql(&glob_dir.join("10-first.sql"));
    let glob = format!("{}/*.sql", glob_dir.to_string_lossy().replace('\\', "/"));
    write_test(
        &suite.join("absolute.test.yaml"),
        "absolute fixture selectors",
        &format!(
            "    - sql_file: {}\n    - sql_glob: {}\n",
            yaml_path(&exact),
            yaml_string(&glob)
        ),
    );

    let output = run_validate(command_cwd.path(), &suite);
    assert_exit(&output, 0);
}

#[test]
fn validate_resolves_environment_derived_fixture_paths() {
    let workspace = TempDir::new("environment-path");
    let command_cwd = TempDir::new("environment-path-cwd");
    let suite = workspace.path().join("suite");
    let external = workspace.path().join("environment-fixtures");
    write_vault(&suite, "");
    write_sql(&external.join("selected.sql"));
    write_test(
        &suite.join("environment.test.yaml"),
        "environment fixture selector",
        "    - sql_file: ${env.VAULT_CLI_FIXTURE_ROOT}/selected.sql\n",
    );

    let output = vault_command(command_cwd.path())
        .env("VAULT_CLI_FIXTURE_ROOT", &external)
        .arg("validate")
        .arg("--suite-dir")
        .arg(&suite)
        .output()
        .expect("vault subprocess should start");
    assert_exit(&output, 0);
}

#[test]
fn sql_file_treats_wildcards_as_literal_while_sql_glob_expands_them() {
    let workspace = TempDir::new("exact-versus-glob");
    let command_cwd = TempDir::new("exact-versus-glob-cwd");

    let exact_suite = workspace.path().join("exact-suite");
    write_vault(&exact_suite, "");
    write_sql(&exact_suite.join("fixtures/match-one.sql"));
    write_test(
        &exact_suite.join("exact.test.yaml"),
        "exact wildcard fixture",
        "    - sql_file: fixtures/match-*.sql\n",
    );
    let exact = run_validate(command_cwd.path(), &exact_suite);
    assert_exit(&exact, 2);
    let exact_stderr = text(&exact.stderr);
    assert!(
        exact_stderr.contains("fixtures/match-*.sql"),
        "{exact_stderr}"
    );
    assert!(
        exact_stderr.contains("could not resolve SQL file"),
        "{exact_stderr}"
    );

    let glob_suite = workspace.path().join("glob-suite");
    write_vault(&glob_suite, "");
    write_sql(&glob_suite.join("fixtures/match-one.sql"));
    write_test(
        &glob_suite.join("glob.test.yaml"),
        "glob wildcard fixture",
        "    - sql_glob: fixtures/match-*.sql\n",
    );
    let glob = run_validate(command_cwd.path(), &glob_suite);
    assert_exit(&glob, 0);
}

#[test]
fn zero_match_glob_fails_before_any_external_connection() {
    let workspace = TempDir::new("zero-match");
    let command_cwd = TempDir::new("zero-match-cwd");
    let suite = workspace.path().join("suite");

    write_vault(&suite, "");
    fs::create_dir_all(suite.join("fixtures")).expect("glob prefix should exist");
    write_test(
        &suite.join("zero.test.yaml"),
        "zero match fixture",
        "    - sql_glob: fixtures/*.sql\n",
    );

    let output = run_suite(command_cwd.path(), &suite);
    assert_exit(&output, 2);
    let stderr = text(&output.stderr);
    assert!(stderr.contains("matched no files"), "{stderr}");
    assert!(
        !stderr.contains("environment error") && !stderr.contains("connection error"),
        "selector validation unexpectedly reached external setup:\n{stderr}"
    );
}

#[test]
fn empty_selection_does_not_resolve_unselected_fixture_files() {
    let workspace = TempDir::new("empty-selection");
    let command_cwd = TempDir::new("empty-selection-cwd");
    let suite = workspace.path().join("suite");

    write_vault(
        &suite,
        "seed:\n  postgres:\n    - sql_file: fixtures/global-does-not-exist.sql\n",
    );
    write_test(
        &suite.join("unselected.test.yaml"),
        "unselected fixture",
        "    - sql_file: fixtures/does-not-exist.sql\n",
    );

    let output = vault_command(command_cwd.path())
        .arg("run")
        .arg("nothing-matches-this-pattern")
        .arg("--suite-dir")
        .arg(&suite)
        .output()
        .expect("vault subprocess should start");
    assert_exit(&output, 2);
    let stderr = text(&output.stderr);
    assert!(stderr.contains("no tests or flows matched"), "{stderr}");
    assert!(!stderr.contains("does-not-exist.sql"), "{stderr}");
    assert!(!stderr.contains("global-does-not-exist.sql"), "{stderr}");
}

#[test]
fn validate_rejects_a_canonical_duplicate_across_global_and_local_seed() {
    let workspace = TempDir::new("global-local-duplicate");
    let command_cwd = TempDir::new("global-local-duplicate-cwd");
    let suite = workspace.path().join("suite");

    write_sql(&suite.join("fixtures/shared.sql"));
    write_vault(
        &suite,
        "seed:\n  postgres:\n    - sql_file: fixtures/shared.sql\n",
    );
    write_test(
        &suite.join("cases/duplicate.test.yaml"),
        "duplicate fixture",
        "    - sql_file: ../fixtures/./shared.sql\n",
    );

    let output = run_validate(command_cwd.path(), &suite);
    assert_exit(&output, 2);
    let stderr = text(&output.stderr);
    assert!(stderr.contains("already selected"), "{stderr}");
    assert!(stderr.contains("fixtures/shared.sql"), "{stderr}");
    assert!(stderr.contains("duplicate.test.yaml"), "{stderr}");
}

#[test]
fn list_fixtures_shows_origin_scope_paths_and_stable_execution_order() {
    let workspace = TempDir::new("list-fixtures");
    let command_cwd = TempDir::new("list-fixtures-cwd");
    let suite = workspace.path().join("suite");
    let test_path = suite.join("cases/deep/fixture-plan.test.yaml");
    let global_exact = suite.join("fixtures/00-global.sql");
    let glob_first = suite.join("fixtures/glob/10-first.sql");
    let glob_second = suite.join("fixtures/glob/20-second.sql");
    let local_exact = suite.join("fixtures/99-local.sql");

    for fixture in [&global_exact, &glob_first, &glob_second, &local_exact] {
        write_sql(fixture);
    }
    write_vault(
        &suite,
        concat!(
            "seed:\n",
            "  postgres:\n",
            "    - sql_file: fixtures/00-global.sql\n",
            "    - sql_glob: fixtures/glob/*.sql\n"
        ),
    );
    write_test(
        &test_path,
        "fixture execution plan",
        "    - sql_file: ../../fixtures/99-local.sql\n",
    );

    let output = run_list_with_fixtures(command_cwd.path(), &suite);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);

    assert!(stdout.contains("resolved SQL fixtures"), "{stdout}");
    assert!(stdout.contains("postgres [global] sql_file"), "{stdout}");
    assert!(stdout.contains("postgres [global] sql_glob"), "{stdout}");
    assert!(stdout.contains("postgres [local] sql_file"), "{stdout}");
    assert!(
        stdout.contains("vault.yaml"),
        "missing global YAML origin:\n{stdout}"
    );
    assert!(
        stdout.contains("fixture-plan.test.yaml"),
        "missing local YAML origin:\n{stdout}"
    );
    assert!(
        stdout.contains("fixtures/glob/*.sql"),
        "missing original glob selector:\n{stdout}"
    );

    let global_resolved = global_exact
        .canonicalize()
        .expect("global fixture should canonicalize");
    let local_resolved = local_exact
        .canonicalize()
        .expect("local fixture should canonicalize");
    assert!(
        stdout.contains(&global_resolved.display().to_string()),
        "missing resolved global path:\n{stdout}"
    );
    assert!(
        stdout.contains(&local_resolved.display().to_string()),
        "missing resolved local path:\n{stdout}"
    );
    assert_in_order(
        &stdout,
        &[
            "fixtures/00-global.sql",
            "fixtures/glob/10-first.sql",
            "fixtures/glob/20-second.sql",
            "../../fixtures/99-local.sql",
        ],
    );
}

#[test]
fn list_fixtures_reflects_reset_once_and_defers_global_seed_past_skipped_stages() {
    let workspace = TempDir::new("list-reset-once");
    let command_cwd = TempDir::new("list-reset-once-cwd");
    let suite = workspace.path().join("suite");

    for name in ["00-global.sql", "20-first.sql", "30-later.sql"] {
        write_sql(&suite.join("fixtures").join(name));
    }
    write_vault(
        &suite,
        "seed:\n  postgres:\n    - sql_file: fixtures/00-global.sql\n",
    );
    write_test(
        &suite.join("skipped.test.yaml"),
        "skipped stage",
        "    - sql_file: fixtures/10-skipped.sql\n",
    );
    let skipped_path = suite.join("skipped.test.yaml");
    let skipped = fs::read_to_string(&skipped_path).unwrap().replacen(
        "test: skipped stage\n",
        "test: skipped stage\nskip: demonstration\n",
        1,
    );
    fs::write(&skipped_path, skipped).unwrap();
    write_test(
        &suite.join("first.test.yaml"),
        "first executable stage",
        "    - sql_file: fixtures/20-first.sql\n",
    );
    write_test(
        &suite.join("later.test.yaml"),
        "later stage",
        "    - sql_file: fixtures/30-later.sql\n",
    );
    fs::write(
        suite.join("ordered.flow.yaml"),
        r#"flow: ordered fixtures
reset: once
stages:
  - test: skipped stage
  - test: first executable stage
  - test: later stage
"#,
    )
    .unwrap();

    let output = run_list_with_fixtures(command_cwd.path(), &suite);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);

    assert!(
        stdout.contains("1. skipped stage [skipped; no fixture execution]"),
        "{stdout}"
    );
    assert_eq!(
        stdout.matches("[global]").count(),
        1,
        "reset-once should list the global fixture only with the first executable stage:\n{stdout}"
    );
    assert!(!stdout.contains("10-skipped.sql"), "{stdout}");
    assert_in_order(
        &stdout,
        &[
            "2. first executable stage",
            "fixtures/00-global.sql",
            "fixtures/20-first.sql",
            "3. later stage",
            "fixtures/30-later.sql",
        ],
    );
}

#[test]
fn list_fixtures_marks_later_reset_once_global_plans_as_conditional() {
    let workspace = TempDir::new("list-reset-once-conditional");
    let command_cwd = TempDir::new("list-reset-once-conditional-cwd");
    let suite = workspace.path().join("suite");

    for name in ["00-global.sql", "10-first.sql", "20-fallback.sql"] {
        write_sql(&suite.join("fixtures").join(name));
    }
    write_vault(
        &suite,
        "seed:\n  postgres:\n    - sql_file: fixtures/00-global.sql\n",
    );
    write_test(
        &suite.join("first.test.yaml"),
        "first stage",
        "    - sql_file: fixtures/10-first.sql\n",
    );
    write_test(
        &suite.join("fallback.test.yaml"),
        "fallback stage",
        "    - sql_file: fixtures/20-fallback.sql\n",
    );
    fs::write(
        suite.join("conditional.flow.yaml"),
        r#"flow: conditional initialization
reset: once
on_failure: continue
stages:
  - test: first stage
    with: { broken: "{{ ??? }}" }
  - test: fallback stage
"#,
    )
    .unwrap();

    let output = run_list_with_fixtures(command_cwd.path(), &suite);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);

    assert_eq!(
        stdout.matches("[global]").count(),
        2,
        "both possible reset-bound plans must include the global fixture:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "2. fallback stage [global fixtures run only if initialization is still pending]"
        ),
        "{stdout}"
    );
    assert_in_order(
        &stdout,
        &[
            "1. first stage",
            "fixtures/00-global.sql",
            "fixtures/10-first.sql",
            "2. fallback stage [global fixtures run only if initialization is still pending]",
            "fixtures/00-global.sql",
            "fixtures/20-fallback.sql",
        ],
    );
}
