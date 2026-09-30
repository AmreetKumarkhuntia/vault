use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use vault_store::{
    DocMode, PreparedStoreDoc, StoreConnConfig, StoreDocContext, StoreDocScope, StoreDriver,
};
use vault_store_postgres::PostgresDriver;

static TEMP_DIR_ID: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let id = TEMP_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vault-postgres-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temporary fixture directory should be created");
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

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sql_file/suite")
        .canonicalize()
        .expect("SQL fixture suite root should exist")
}

fn validate_at(doc: &Value, root: &Path) -> Result<(), String> {
    PostgresDriver
        .validate_with_suite_root(doc, DocMode::Seed, root)
        .map_err(|error| error.to_string())
}

fn prepare_at(
    doc: &Value,
    suite_root: &Path,
    declaring_yaml: &Path,
    scope: StoreDocScope,
) -> Result<PreparedStoreDoc, String> {
    PostgresDriver
        .prepare_with_context(
            doc,
            DocMode::Seed,
            &StoreDocContext::new(suite_root, declaring_yaml, scope),
        )
        .map_err(|error| error.to_string())
}

fn assert_invalid_at(doc: Value, root: &Path, expected: &[&str]) -> String {
    let error = validate_at(&doc, root).expect_err("seed document should be rejected");
    for needle in expected {
        assert!(
            error.contains(needle),
            "expected `{needle}` in validation error:\n{error}"
        );
    }
    error
}

fn assert_invalid(doc: Value, expected: &[&str]) -> String {
    assert_invalid_at(doc, &fixture_root(), expected)
}

#[test]
fn accepts_each_well_formed_seed_entry_variant() {
    let doc = json!([
        {
            "table": "vault_sql_file_fixture_cases",
            "rows": [{ "id": 1, "label": "structured" }],
            "conflict": "error"
        },
        { "sql": "SELECT 1" },
        { "sql_file": "./fixtures/multi_statement.sql" },
        { "sql_glob": "fixtures/transaction_keywords*.sql" }
    ]);

    validate_at(&doc, &fixture_root()).expect("valid seed entry variants should validate");
}

#[test]
fn legacy_validation_stays_filesystem_independent_and_allows_external_references() {
    PostgresDriver
        .validate(
            &json!([{ "sql_file": "fixtures/not-present-yet.sql" }]),
            DocMode::Seed,
        )
        .expect("legacy validation must not require suite-root filesystem access");

    PostgresDriver
        .validate(
            &json!([
                { "sql_file": "../../outside.sql" },
                { "sql_file": "/opt/company/fixture.sql" },
                { "sql_glob": "../../fixtures/**/*.sql" },
                { "sql_file": "C:/fixtures/windows.sql" },
                { "sql_glob": "C:/fixtures/**/*.sql" },
                { "sql_file": "//server/share/fixtures/unc.sql" }
            ]),
            DocMode::Seed,
        )
        .expect("legacy validation accepts parent, absolute, and glob selectors without I/O");

    let missing_root = std::env::temp_dir().join(format!(
        "vault-postgres-missing-context-{}",
        std::process::id()
    ));
    PostgresDriver
        .prepare_with_context(
            &json!([{ "sql": "SELECT 1" }]),
            DocMode::Seed,
            &StoreDocContext::new(
                &missing_root,
                missing_root.join("nested/test.yaml"),
                StoreDocScope::Local,
            ),
        )
        .expect("inline and structured seeds must not require a filesystem context");

    let absolute_temp = TempDir::new("absolute-with-missing-origin");
    let absolute_file = absolute_temp.path().join("external.sql");
    fs::write(&absolute_file, "SELECT 1;").unwrap();
    PostgresDriver
        .prepare_with_context(
            &json!([{ "sql_file": absolute_file }]),
            DocMode::Seed,
            &StoreDocContext::new(
                &missing_root,
                missing_root.join("nested/test.yaml"),
                StoreDocScope::Local,
            ),
        )
        .expect("an absolute selector must not depend on the declaring directory");
}

#[cfg(not(windows))]
#[test]
fn preparation_rejects_foreign_windows_absolute_forms_without_reinterpreting_them() {
    let temp = TempDir::new("foreign-windows-paths");
    let suite = temp.path().join("suite");
    fs::create_dir_all(&suite).unwrap();

    for selector in [
        "C:/fixtures/windows.sql",
        r"C:\fixtures\windows.sql",
        "//server/share/fixtures/unc.sql",
        r"\\server\share\fixtures\unc.sql",
    ] {
        let error = prepare_at(
            &json!([{ "sql_file": selector }]),
            &suite,
            &suite.join("vault.yaml"),
            StoreDocScope::Global,
        )
        .expect_err("a foreign absolute path must not become a local relative or root path");
        assert!(error.contains(selector), "{error}");
        assert!(error.contains("Windows absolute path"), "{error}");
    }

    for selector in ["C:/fixtures/**/*.sql", "//server/share/fixtures/*.sql"] {
        let error = prepare_at(
            &json!([{ "sql_glob": selector }]),
            &suite,
            &suite.join("vault.yaml"),
            StoreDocScope::Global,
        )
        .expect_err("a foreign absolute glob must not be traversed locally");
        assert!(error.contains(selector), "{error}");
        assert!(error.contains("Windows absolute path"), "{error}");
    }

    let drive_relative = prepare_at(
        &json!([{ "sql_file": "C:fixtures/not-absolute.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect_err("drive-relative paths are ambiguous and must be rejected");
    assert!(drive_relative.contains("drive paths must be absolute"));
}

#[cfg(windows)]
#[test]
fn preparation_rejects_foreign_unix_absolute_forms_without_reinterpreting_them() {
    let temp = TempDir::new("foreign-unix-paths");
    let suite = temp.path().join("suite");
    fs::create_dir_all(&suite).unwrap();

    let error = prepare_at(
        &json!([{ "sql_file": "/opt/company/fixtures/base.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect_err("a Unix root must not be interpreted relative to a Windows drive");
    assert!(error.contains("Unix absolute path"), "{error}");
}

#[test]
fn rejects_malformed_seed_entries_and_mixed_variants() {
    let cases = [
        (json!({}), vec!["seed.postgres", "must be a list"]),
        (
            json!(["SELECT 1"]),
            vec!["seed.postgres[0]", "must be a mapping"],
        ),
        (
            json!([{ "unknown": true }]),
            vec!["seed.postgres[0]", "unknown key `unknown`"],
        ),
        (
            json!([{}]),
            vec![
                "seed.postgres[0]",
                "exactly one",
                "table",
                "sql",
                "sql_file",
                "sql_glob",
            ],
        ),
        (
            json!([{ "table": "items", "sql": "SELECT 1", "rows": [] }]),
            vec!["seed.postgres[0]", "exactly one"],
        ),
        (
            json!([{ "sql": "SELECT 1", "sql_file": "fixtures/multi_statement.sql" }]),
            vec!["seed.postgres[0]", "exactly one"],
        ),
        (
            json!([{ "sql_file": "fixtures/multi_statement.sql", "sql_glob": "fixtures/*.sql" }]),
            vec!["seed.postgres[0]", "exactly one"],
        ),
        (
            json!([{ "table": 42, "rows": [] }]),
            vec!["seed.postgres[0].table", "must be a string"],
        ),
        (
            json!([{ "table": "  ", "rows": [] }]),
            vec!["seed.postgres[0].table", "must not be empty"],
        ),
        (
            json!([{ "table": "items" }]),
            vec!["seed.postgres[0].rows", "must be a list"],
        ),
        (
            json!([{ "table": "items", "rows": {} }]),
            vec!["seed.postgres[0].rows", "must be a list"],
        ),
        (
            json!([{ "table": "items", "rows": [1] }]),
            vec!["seed.postgres[0].rows[0]", "must be a mapping"],
        ),
        (
            json!([{ "table": "items", "rows": [], "conflict": false }]),
            vec!["seed.postgres[0].conflict", "must be a string"],
        ),
        (
            json!([{ "sql": 42 }]),
            vec!["seed.postgres[0].sql", "must be a string"],
        ),
        (
            json!([{ "sql": " \n\t" }]),
            vec!["seed.postgres[0].sql", "must not be empty"],
        ),
        (
            json!([{ "sql_file": 42 }]),
            vec!["seed.postgres[0].sql_file", "must be a string"],
        ),
        (
            json!([{ "sql_file": "  " }]),
            vec!["seed.postgres[0].sql_file", "must not be empty"],
        ),
        (
            json!([{ "sql_glob": 42 }]),
            vec!["seed.postgres[0].sql_glob", "must be a string"],
        ),
        (
            json!([{ "sql_glob": "fixtures/file.sql" }]),
            vec!["seed.postgres[0].sql_glob", "must contain"],
        ),
        (
            json!([{ "sql": "SELECT 1", "rows": [] }]),
            vec!["seed.postgres[0].rows", "only valid with `table`"],
        ),
        (
            json!([{ "sql_file": "fixtures/multi_statement.sql", "conflict": "ignore" }]),
            vec!["seed.postgres[0].conflict", "only valid with `table`"],
        ),
        (
            json!([{ "sql_glob": "fixtures/*.sql", "rows": [] }]),
            vec!["seed.postgres[0].rows", "only valid with `table`"],
        ),
    ];

    for (doc, expected) in cases {
        assert_invalid(doc, &expected);
    }
}

#[test]
fn exact_files_allow_parent_and_absolute_paths_but_still_validate_the_target() {
    let root = fixture_root();
    let absolute = root.join("fixtures/multi_statement.sql");
    validate_at(&json!([{ "sql_file": absolute }]), &root)
        .expect("absolute SQL paths are supported");

    let temp = TempDir::new("declaration-relative");
    let suite = temp.path().join("suite");
    let cases_dir = suite.join("tests/orders");
    let outside = temp.path().join("shared/base.sql");
    fs::create_dir_all(&cases_dir).unwrap();
    fs::create_dir_all(outside.parent().unwrap()).unwrap();
    fs::write(&outside, "SELECT 1;").unwrap();
    let declaring_yaml = cases_dir.join("create.test.yaml");
    let prepared = prepare_at(
        &json!([{ "sql_file": "../../../shared/base.sql" }]),
        &suite,
        &declaring_yaml,
        StoreDocScope::Local,
    )
    .expect("parent traversal resolves from the declaring YAML directory");
    assert_eq!(prepared.files().len(), 1);
    assert_eq!(
        prepared.files()[0].resolved_path,
        outside.canonicalize().unwrap()
    );
    assert_eq!(prepared.files()[0].declaring_yaml, declaring_yaml);
    assert_eq!(prepared.files()[0].scope, StoreDocScope::Local);

    let invalid_cases = [
        (
            json!([{ "sql_file": "fixtures/missing.sql" }]),
            vec![
                "seed.postgres[0].sql_file",
                "fixtures/missing.sql",
                "could not resolve SQL file",
            ],
        ),
        (
            json!([{ "sql_file": "fixtures/not_sql.txt" }]),
            vec!["seed.postgres[0].sql_file", "`.sql` extension"],
        ),
        (
            json!([{ "sql_file": "fixtures/MIGRATION.SQL" }]),
            vec!["seed.postgres[0].sql_file", "`.sql` extension"],
        ),
        (
            json!([{ "sql_file": "fixtures/{{ vars.fixture }}.sql" }]),
            vec!["seed.postgres[0].sql_file", "static path", "template"],
        ),
        (
            json!([{ "sql_file": "fixtures" }]),
            vec!["seed.postgres[0].sql_file", "`.sql` extension"],
        ),
    ];

    for (doc, expected) in invalid_cases {
        assert_invalid_at(doc, &root, &expected);
    }

    let temp = TempDir::new("directory-with-sql-extension");
    let suite = temp.path().join("suite");
    fs::create_dir_all(suite.join("fixtures/a-directory.sql")).unwrap();
    assert_invalid_at(
        json!([{ "sql_file": "fixtures/a-directory.sql" }]),
        &suite,
        &["seed.postgres[0].sql_file", "regular file"],
    );
}

#[cfg(unix)]
#[test]
fn allows_file_symlinks_to_targets_inside_or_outside_the_suite() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new("symlinks");
    let suite = temp.path().join("suite");
    let fixtures = suite.join("fixtures");
    fs::create_dir_all(&fixtures).unwrap();

    let target = fixtures.join("target.sql");
    fs::write(&target, "SELECT 1;").unwrap();
    symlink("target.sql", fixtures.join("internal.sql")).unwrap();
    validate_at(&json!([{ "sql_file": "fixtures/internal.sql" }]), &suite)
        .expect("a symlink whose canonical target stays inside the suite is safe");

    let outside = temp.path().join("outside.sql");
    fs::write(&outside, "SELECT 1;").unwrap();
    symlink(&outside, fixtures.join("escape.sql")).unwrap();
    let prepared = prepare_at(
        &json!([{ "sql_file": "fixtures/escape.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect("filesystem-wide references include symlink targets outside the suite");
    assert_eq!(
        prepared.files()[0].resolved_path,
        outside.canonicalize().unwrap()
    );
}

#[cfg(unix)]
#[test]
fn relative_candidates_preserve_a_symlinked_declaring_directory() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new("symlinked-declaration-base");
    let suite = temp.path().join("suite");
    let first = temp.path().join("declaration-one");
    let second = temp.path().join("declaration-two");
    let linked = suite.join("linked-cases");
    fs::create_dir_all(&suite).unwrap();
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    fs::write(first.join("fixture.sql"), "SELECT 1;").unwrap();
    fs::write(second.join("fixture.sql"), "SELECT 2;").unwrap();
    symlink(&first, &linked).unwrap();

    let exact = prepare_at(
        &json!([{ "sql_file": "fixture.sql" }]),
        &suite,
        &linked.join("case.test.yaml"),
        StoreDocScope::Local,
    )
    .expect("relative exact files should resolve through the declaration path");
    let glob = prepare_at(
        &json!([{ "sql_glob": "*.sql" }]),
        &suite,
        &linked.join("case.test.yaml"),
        StoreDocScope::Local,
    )
    .expect("relative globs should resolve through the declaration path");

    for prepared in [&exact, &glob] {
        assert_eq!(
            prepared.files()[0].candidate_path,
            linked.join("fixture.sql")
        );
        assert_eq!(
            prepared.files()[0].resolved_path,
            first.join("fixture.sql").canonicalize().unwrap()
        );
    }

    fs::remove_file(&linked).unwrap();
    symlink(&second, &linked).unwrap();
    assert_ne!(
        exact.files()[0].resolved_path,
        exact.files()[0].candidate_path.canonicalize().unwrap(),
        "retargeting the declaring-directory symlink must change the runtime identity"
    );
}

#[test]
fn glob_expansion_is_declaration_relative_sorted_and_spliced_in_place() {
    let temp = TempDir::new("glob-order");
    let suite = temp.path().join("suite");
    let declaring_dir = suite.join("tests/orders");
    let fixture_dir = temp.path().join("shared/fixtures");
    fs::create_dir_all(&declaring_dir).unwrap();
    fs::create_dir_all(fixture_dir.join("nested")).unwrap();
    fs::write(fixture_dir.join("20_second.sql"), "SELECT 20;").unwrap();
    fs::write(fixture_dir.join("10_first.sql"), "SELECT 10;").unwrap();
    fs::write(fixture_dir.join("nested/05_nested.sql"), "SELECT 5;").unwrap();
    let declaring_yaml = declaring_dir.join("create.test.yaml");

    let prepared = prepare_at(
        &json!([
            { "sql": "SELECT 0" },
            { "sql_glob": "../../../shared/fixtures/*.sql" },
            { "table": "items", "rows": [] }
        ]),
        &suite,
        &declaring_yaml,
        StoreDocScope::Local,
    )
    .expect("a nonrecursive glob should resolve outside the suite");

    assert_eq!(prepared.doc().as_array().unwrap().len(), 4);
    assert_eq!(
        prepared
            .files()
            .iter()
            .map(|source| (source.source_index, source.prepared_index))
            .collect::<Vec<_>>(),
        [(1, 1), (1, 2)],
        "glob matches are inserted at the selector's position"
    );
    assert_eq!(
        prepared
            .files()
            .iter()
            .map(|source| source.logical_path.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        [
            "../../../shared/fixtures/10_first.sql",
            "../../../shared/fixtures/20_second.sql",
        ],
        "logical paths determine deterministic ordering and `*` does not cross `/`"
    );

    let recursive = prepare_at(
        &json!([{ "sql_glob": "../../../shared/fixtures/**/*.sql" }]),
        &suite,
        &declaring_yaml,
        StoreDocScope::Local,
    )
    .expect("`**` should select recursively");
    assert_eq!(recursive.files().len(), 3);
    assert!(recursive
        .files()
        .iter()
        .any(|source| source.logical_path.ends_with("nested/05_nested.sql")));
}

#[test]
fn relative_globs_treat_metacharacters_in_ancestor_directories_as_literals() {
    let temp = TempDir::new("glob-literal-ancestor");
    let suite = temp.path().join("suite[1]{literal}*?");
    let fixtures = suite.join("fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    fs::write(fixtures.join("10-first.sql"), "SELECT 1;").unwrap();
    fs::write(fixtures.join("20-second.sql"), "SELECT 2;").unwrap();

    let prepared = prepare_at(
        &json!([{ "sql_glob": "fixtures/*.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect("only the YAML selector, not its literal absolute base, is glob syntax");

    assert_eq!(prepared.files().len(), 2);
    assert!(prepared.files()[0].logical_path.ends_with("10-first.sql"));
    assert!(prepared.files()[1].logical_path.ends_with("20-second.sql"));
}

#[test]
fn glob_matches_are_sorted_before_file_validation() {
    let temp = TempDir::new("glob-validation-order");
    let suite = temp.path().join("suite");
    let fixtures = suite.join("fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    fs::write(fixtures.join("20-later.sql"), "ROLLBACK;").unwrap();
    fs::write(fixtures.join("10-first.sql"), "COMMIT;").unwrap();

    let error = prepare_at(
        &json!([{ "sql_glob": "fixtures/*.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect_err("the first normalized logical match should fail first");
    assert!(error.contains("fixtures/10-first.sql"), "{error}");
    assert!(error.contains("COMMIT"), "{error}");
    assert!(!error.contains("20-later.sql"), "{error}");
}

#[test]
fn glob_selection_is_frozen_and_exact_file_treats_wildcards_literally() {
    let temp = TempDir::new("glob-freeze");
    let suite = temp.path().join("suite");
    let fixtures = suite.join("fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    fs::write(fixtures.join("10.sql"), "SELECT 10;").unwrap();
    fs::write(fixtures.join("literal*.sql"), "SELECT 1;").unwrap();

    let prepared = prepare_at(
        &json!([{ "sql_glob": "fixtures/1?.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect("`?` should match exactly one character");
    assert_eq!(prepared.files().len(), 1);
    fs::write(fixtures.join("11.sql"), "SELECT 11;").unwrap();
    assert_eq!(
        prepared.files().len(),
        1,
        "new files do not alter an already prepared execution document"
    );

    let literal = prepare_at(
        &json!([{ "sql_file": "fixtures/literal*.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect("wildcards are literal characters in sql_file");
    assert_eq!(literal.files().len(), 1);
    assert_eq!(literal.files()[0].selector_kind, "sql_file");
}

#[test]
fn glob_errors_are_strict_and_name_the_origin_and_selector() {
    let temp = TempDir::new("glob-errors");
    let suite = temp.path().join("suite");
    let fixtures = suite.join("fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    fs::write(fixtures.join("UPPER.SQL"), "SELECT 1;").unwrap();
    let origin = suite.join("nested/case.test.yaml");
    fs::create_dir_all(origin.parent().unwrap()).unwrap();

    for (selector, expected) in [
        ("../fixtures/no-match-*.sql", "matched no files"),
        ("../fixtures/file.sql", "must contain"),
        ("../fixtures/[ab].sql", "supports only"),
        ("..\\fixtures\\*.sql", "must use `/`"),
        ("../fixtures/***.sql", "invalid `***`"),
        ("../fixtures/**.sql", "complete path segment"),
        ("../fixtures/name**/*.sql", "complete path segment"),
        (
            "../fixtures/*/../../shared/*.sql",
            "before the first wildcard",
        ),
    ] {
        let error = prepare_at(
            &json!([{ "sql_glob": selector }]),
            &suite,
            &origin,
            StoreDocScope::Local,
        )
        .expect_err("invalid or empty globs are configuration errors");
        assert!(error.contains("seed.postgres[0].sql_glob"), "{error}");
        assert!(error.contains(selector), "{error}");
        assert!(error.contains(expected), "{error}");
    }

    let extension_error = prepare_at(
        &json!([{ "sql_glob": "../fixtures/*" }]),
        &suite,
        &origin,
        StoreDocScope::Local,
    )
    .expect_err("every glob match must be a lowercase .sql file");
    for expected in ["seed.postgres[0].sql_glob", "UPPER.SQL", "lowercase `.sql`"] {
        assert!(extension_error.contains(expected), "{extension_error}");
    }
}

#[test]
fn rejects_runtime_template_markers_introduced_by_a_glob_match() {
    let temp = TempDir::new("glob-template-match");
    let suite = temp.path().join("suite");
    let fixtures = suite.join("fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    fs::write(fixtures.join("{{ env.OTHER_FILE }}.sql"), "SELECT 1;").unwrap();

    let error = prepare_at(
        &json!([{ "sql_glob": "fixtures/*.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect_err("glob matches must not introduce runtime template expressions");
    for expected in [
        "seed.postgres[0].sql_glob",
        "{{ env.OTHER_FILE }}.sql",
        "runtime template marker",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn rejects_runtime_template_markers_in_a_symlink_target_path() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new("symlink-template-target");
    let suite = temp.path().join("suite");
    let fixtures = suite.join("fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    let target = temp.path().join("{{ env.OTHER_TARGET }}.sql");
    fs::write(&target, "SELECT 1;").unwrap();
    symlink(&target, fixtures.join("safe-name.sql")).unwrap();

    let error = prepare_at(
        &json!([{ "sql_file": "fixtures/safe-name.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect_err("a canonical target must not introduce runtime template expressions");
    for expected in [
        "seed.postgres[0].sql_file",
        "{{ env.OTHER_TARGET }}.sql",
        "runtime template marker",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn canonical_duplicates_fail_with_both_selector_origins() {
    let temp = TempDir::new("duplicates");
    let suite = temp.path().join("suite");
    let fixtures = suite.join("fixtures");
    let nested = suite.join("tests");
    fs::create_dir_all(&fixtures).unwrap();
    fs::create_dir_all(&nested).unwrap();
    fs::write(fixtures.join("same.sql"), "SELECT 1;").unwrap();

    let within = prepare_at(
        &json!([
            { "sql_file": "fixtures/same.sql" },
            { "sql_glob": "fixtures/*.sql" }
        ]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect_err("overlapping selectors cannot execute one canonical file twice");
    for expected in [
        "seed.postgres[1].sql_glob",
        "already selected",
        "sql_file",
        "same.sql",
    ] {
        assert!(within.contains(expected), "{within}");
    }

    let mut global = prepare_at(
        &json!([{ "sql_file": "fixtures/same.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .unwrap();
    let local_origin = nested.join("case.test.yaml");
    let local = prepare_at(
        &json!([{ "sql_file": "../fixtures/same.sql" }]),
        &suite,
        &local_origin,
        StoreDocScope::Local,
    )
    .unwrap();
    let across = global
        .append(local)
        .expect_err("global/local duplicate identities must fail")
        .to_string();
    for expected in ["seed.postgres[0].sql_file", "already selected", "same.sql"] {
        assert!(across.contains(expected), "{across}");
    }
}

#[cfg(unix)]
#[test]
fn glob_follows_a_symlinked_fixed_prefix_but_not_discovered_directory_symlinks() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new("glob-symlink-walk");
    let suite = temp.path().join("suite");
    let fixtures = suite.join("fixtures");
    let external = temp.path().join("external");
    fs::create_dir_all(&fixtures).unwrap();
    fs::create_dir_all(&external).unwrap();
    fs::write(fixtures.join("visible.sql"), "SELECT 1;").unwrap();
    fs::write(external.join("external.sql"), "SELECT 2;").unwrap();
    symlink(&external, suite.join("linked-prefix")).unwrap();
    symlink(&external, fixtures.join("discovered-link")).unwrap();

    let followed = prepare_at(
        &json!([{ "sql_glob": "linked-prefix/*.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect("a symlink in the fixed prefix is resolved before walking");
    assert_eq!(followed.files().len(), 1);
    assert_eq!(
        followed.files()[0].resolved_path,
        external.join("external.sql").canonicalize().unwrap()
    );
    assert_eq!(
        followed.files()[0].candidate_path,
        suite.join("linked-prefix/external.sql"),
        "runtime identity checks must re-resolve through the original symlinked prefix"
    );

    let not_followed = prepare_at(
        &json!([{ "sql_glob": "fixtures/**/*.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect("a discovered directory symlink should be ignored without aborting the walk");
    assert_eq!(not_followed.files().len(), 1);
    assert!(not_followed.files()[0]
        .logical_path
        .ends_with("visible.sql"));
}

#[test]
fn missing_declaration_relative_file_reports_the_legacy_suite_root_hint() {
    let temp = TempDir::new("legacy-hint");
    let suite = temp.path().join("suite");
    let old_fixtures = suite.join("fixtures");
    let nested = suite.join("tests/orders");
    fs::create_dir_all(&old_fixtures).unwrap();
    fs::create_dir_all(&nested).unwrap();
    fs::write(old_fixtures.join("old.sql"), "SELECT 1;").unwrap();

    let error = prepare_at(
        &json!([{ "sql_file": "fixtures/old.sql" }]),
        &suite,
        &nested.join("case.test.yaml"),
        StoreDocScope::Local,
    )
    .expect_err("nested documents no longer resolve paths from the suite root");
    for expected in [
        "seed.postgres[0].sql_file",
        "declaring YAML",
        "former suite-root location exists",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn glob_rejects_non_utf8_matched_paths() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let temp = TempDir::new("non-utf8-path");
    let suite = temp.path().join("suite");
    let fixtures = suite.join("fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    let name = OsString::from_vec(b"invalid-\xff.sql".to_vec());
    fs::write(fixtures.join(name), "SELECT 1;").unwrap();

    let error = prepare_at(
        &json!([{ "sql_glob": "fixtures/*.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect_err("matched filesystem paths must be representable in diagnostics");
    assert!(error.contains("not valid UTF-8"), "{error}");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn nonrecursive_glob_does_not_traverse_below_its_pattern_depth() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let temp = TempDir::new("bounded-glob-walk");
    let suite = temp.path().join("suite");
    let fixtures = suite.join("fixtures");
    let nested = fixtures.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(fixtures.join("visible.sql"), "SELECT 1;").unwrap();
    fs::write(
        fixtures.join(OsString::from_vec(b"out-of-scope-\xff.txt".to_vec())),
        "not SQL",
    )
    .unwrap();
    fs::write(
        nested.join(OsString::from_vec(b"out-of-scope-\xff.sql".to_vec())),
        "SELECT 2;",
    )
    .unwrap();

    let prepared = prepare_at(
        &json!([{ "sql_glob": "fixtures/*.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect("a nonrecursive glob must not inspect deeper paths");
    assert_eq!(prepared.files().len(), 1);
    assert!(prepared.files()[0].logical_path.ends_with("visible.sql"));
}

#[test]
fn file_read_errors_name_the_logical_and_resolved_paths() {
    let temp = TempDir::new("invalid-utf8");
    let suite = temp.path().join("suite");
    let fixtures = suite.join("fixtures");
    fs::create_dir_all(&fixtures).unwrap();
    let logical = "fixtures/invalid-utf8.sql";
    let fixture = suite.join(logical);
    fs::write(&fixture, [0xff, 0xfe, 0xfd]).unwrap();
    let resolved = fixture.canonicalize().unwrap();

    assert_invalid_at(
        json!([{ "sql_file": logical }]),
        &suite,
        &[
            "seed.postgres[0].sql_file",
            logical,
            &resolved.display().to_string(),
            "UTF-8",
        ],
    );
}

#[cfg(unix)]
#[test]
fn unreadable_file_is_a_configuration_error() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new("unreadable-file");
    let suite = temp.path().join("suite");
    let fixture = suite.join("fixtures/unreadable.sql");
    fs::create_dir_all(fixture.parent().unwrap()).unwrap();
    fs::write(&fixture, "SELECT 1;").unwrap();
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o000)).unwrap();

    // Privileged test processes can bypass Unix mode bits, in which case this
    // environment cannot model an unreadable fixture reliably.
    if fs::read_to_string(&fixture).is_ok() {
        fs::set_permissions(&fixture, fs::Permissions::from_mode(0o600)).unwrap();
        return;
    }

    let error = prepare_at(
        &json!([{ "sql_file": "fixtures/unreadable.sql" }]),
        &suite,
        &suite.join("vault.yaml"),
        StoreDocScope::Global,
    )
    .expect_err("unreadable selected fixtures must fail preparation");
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o600)).unwrap();
    for expected in [
        "seed.postgres[0].sql_file",
        "unreadable.sql",
        "could not read UTF-8 SQL file",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn rejects_top_level_transaction_control_in_inline_sql_and_files() {
    let cases = [
        ("BEGIN", "BEGIN"),
        ("START /* trivia */ TRANSACTION", "START TRANSACTION"),
        ("COMMIT AND CHAIN", "COMMIT"),
        ("COMMIT PREPARED 'tx-id'", "COMMIT"),
        ("; -- leading trivia\nSELECT 1; cOmMiT", "COMMIT"),
        ("END", "END"),
        ("ROLLBACK TO SAVEPOINT earlier", "ROLLBACK"),
        ("ROLLBACK PREPARED 'tx-id'", "ROLLBACK"),
        ("ABORT", "ABORT"),
        ("SAVEPOINT nested", "SAVEPOINT"),
        ("RELEASE SAVEPOINT nested", "RELEASE"),
        ("RELEASE nested", "RELEASE"),
        ("PREPARE TRANSACTION 'tx-id'", "PREPARE TRANSACTION"),
        ("SET TRANSACTION READ ONLY", "SET TRANSACTION"),
        ("SET LOCAL TRANSACTION READ ONLY", "SET LOCAL TRANSACTION"),
        (
            "SET SESSION TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "SET SESSION TRANSACTION",
        ),
        (
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
            "SET SESSION CHARACTERISTICS AS TRANSACTION",
        ),
        (
            "SET default_transaction_read_only = on",
            "SET transaction configuration",
        ),
        (
            "SET SESSION default_transaction_isolation TO 'serializable'",
            "SET transaction configuration",
        ),
        (
            "SET LOCAL transaction_deferrable = on",
            "SET transaction configuration",
        ),
        (
            "SET transaction_read_only TO off",
            "SET transaction configuration",
        ),
        (
            "RESET default_transaction_deferrable",
            "RESET transaction configuration",
        ),
        (
            "RESET transaction_isolation",
            "RESET transaction configuration",
        ),
        ("RESET ALL", "RESET transaction configuration"),
    ];

    for (sql, command) in cases {
        assert_invalid(
            json!([{ "sql": sql }]),
            &["seed.postgres[0].sql", "transaction-control", command],
        );
    }

    let logical = "fixtures/transaction_control.sql";
    let root = fixture_root();
    let resolved = root.join(logical).canonicalize().unwrap();
    assert_invalid_at(
        json!([{ "sql_file": logical }]),
        &root,
        &[
            "seed.postgres[0].sql_file",
            logical,
            &resolved.display().to_string(),
            "transaction-control",
            "COMMIT",
        ],
    );
}

#[test]
fn transaction_keywords_in_literals_identifiers_comments_and_dollar_bodies_are_data() {
    let inline = r#"
SELECT 'COMMIT; it''s still data; ROLLBACK;';
SELECT E'escaped quote: \'COMMIT\'';
SELECT U&'BEGIN\\0041';
SELECT $$COMMIT; ROLLBACK;$$;
SELECT $MiXeD$BEGIN; $different$ is text; END;$MiXeD$;
SELECT 1 AS "COMMIT", 2 AS "SAVE""POINT";
SELECT 1 AS commitment, 2 AS rollback_count;
SELECT $1;
PREPARE safe_query (integer) AS SELECT $1;
SET LOCAL application_name = 'COMMIT';
-- COMMIT; BEGIN; ROLLBACK;\r
SELECT 1;
/* BEGIN; nested /* SAVEPOINT hidden; */ RELEASE; END; */
DO $vault_body$
BEGIN
    RAISE NOTICE 'COMMIT; ROLLBACK;';
END
$vault_body$;
"#;

    validate_at(
        &json!([
            { "sql": inline },
            { "sql_file": "fixtures/transaction_keywords_as_data.sql" }
        ]),
        &fixture_root(),
    )
    .expect("transaction keywords in protected regions must not be rejected");
}

fn store_config(alias: &str, url: String, suite_root: &Path) -> StoreConnConfig {
    StoreConnConfig {
        alias: alias.into(),
        url,
        options: json!({ "suite_root": suite_root }),
    }
}

async fn reset_live_table(pool: &PgPool) {
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS vault_sql_file_fixture_cases;\
         CREATE TABLE vault_sql_file_fixture_cases (\
           id BIGINT PRIMARY KEY,\
           label TEXT NOT NULL\
         );",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn live_rows(pool: &PgPool) -> Vec<(i64, String)> {
    sqlx::query("SELECT id, label FROM vault_sql_file_fixture_cases ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get("id"), row.get("label")))
        .collect()
}

#[tokio::test]
#[ignore = "requires VAULT_PG_URL (CI provides the PostgreSQL service)"]
async fn live_sql_file_ordering_rollback_and_runtime_mutation_matrix() {
    let database_url = std::env::var("VAULT_PG_URL")
        .expect("VAULT_PG_URL must be set when running the ignored live fixture matrix");
    let root = fixture_root();
    let driver = PostgresDriver;
    let store = driver
        .connect(&store_config("postgres", database_url.clone(), &root))
        .await
        .expect("PostgreSQL fixture test should connect");
    let pool = PgPool::connect(&database_url).await.unwrap();

    reset_live_table(&pool).await;

    let success = json!([
        { "sql_file": "fixtures/multi_statement.sql" },
        {
            "table": "vault_sql_file_fixture_cases",
            "rows": [{ "id": 3, "label": "from-structured-row" }]
        },
        {
            "sql": "UPDATE vault_sql_file_fixture_cases SET label = 'from-inline-sql' WHERE id = 3"
        }
    ]);
    driver
        .validate_with_suite_root(&success, DocMode::Seed, &root)
        .expect("live fixture seed should validate");
    let receipt = store.seed(&success).await.expect("SQL fixture should run");
    assert_eq!(
        receipt.entries,
        [
            "postgres: executed fixtures/multi_statement.sql",
            "postgres: vault_sql_file_fixture_cases +1 rows",
            "postgres: executed inline sql",
        ],
        "seed receipt order should match declaration order"
    );
    assert_eq!(
        live_rows(&pool).await,
        [
            (1, "from-file-one".into()),
            (2, "from-file-two".into()),
            (3, "from-inline-sql".into()),
        ],
        "file, structured, and inline entries should share declaration order"
    );

    sqlx::query("TRUNCATE vault_sql_file_fixture_cases")
        .execute(&pool)
        .await
        .unwrap();
    let failing = json!([
        {
            "sql": "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (10, 'earlier-entry')"
        },
        { "sql_file": "fixtures/invalid.sql" }
    ]);
    driver
        .validate_with_suite_root(&failing, DocMode::Seed, &root)
        .expect("invalid SQL syntax is a runtime concern, not a path error");
    let error = store
        .seed(&failing)
        .await
        .expect_err("invalid fixture SQL should fail the seed transaction")
        .to_string();
    let resolved = root.join("fixtures/invalid.sql").canonicalize().unwrap();
    for needle in [
        "seed.postgres[1].sql_file",
        "fixtures/invalid.sql",
        &resolved.display().to_string(),
    ] {
        assert!(error.contains(needle), "expected `{needle}` in:\n{error}");
    }
    assert!(
        live_rows(&pool).await.is_empty(),
        "all earlier entries and statements from the bad file must roll back"
    );

    let mutation_temp = TempDir::new("runtime-transaction-control");
    let mutation_root = mutation_temp.path().join("suite");
    let mutation_fixtures = mutation_root.join("fixtures");
    fs::create_dir_all(&mutation_fixtures).unwrap();
    let mutation_logical = "fixtures/mutable.sql";
    let mutation_path = mutation_root.join(mutation_logical);
    fs::write(
        &mutation_path,
        "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (30, 'initially-safe');",
    )
    .unwrap();
    let mutation_doc = json!([{ "sql_file": mutation_logical }]);
    let mutation_prepared = driver
        .prepare_with_context(
            &mutation_doc,
            DocMode::Seed,
            &StoreDocContext::new(
                &mutation_root,
                mutation_root.join("vault.yaml"),
                StoreDocScope::Global,
            ),
        )
        .expect("the initial fixture should pass static preparation");
    let mutation_store = driver
        .connect(&store_config(
            "postgres",
            database_url.clone(),
            &mutation_root,
        ))
        .await
        .unwrap();
    fs::write(
        &mutation_path,
        "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (30, 'must-not-commit'); COMMIT; THIS IS INVALID SQL;",
    )
    .unwrap();
    let resolved_mutation = mutation_path.canonicalize().unwrap();
    let mutation_error = mutation_store
        .seed(mutation_prepared.doc())
        .await
        .expect_err("runtime validation must reject a file changed to commit")
        .to_string();
    for needle in [
        "seed.postgres[0].sql_file",
        mutation_logical,
        &resolved_mutation.display().to_string(),
        "COMMIT",
    ] {
        assert!(
            mutation_error.contains(needle),
            "expected `{needle}` in:\n{mutation_error}"
        );
    }
    assert!(
        live_rows(&pool).await.is_empty(),
        "runtime transaction-control rejection must happen before execution"
    );

    let frozen_dir = mutation_root.join("frozen");
    fs::create_dir_all(&frozen_dir).unwrap();
    fs::write(
        frozen_dir.join("10_selected.sql"),
        "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (40, 'frozen-selected');",
    )
    .unwrap();
    let frozen = driver
        .prepare_with_context(
            &json!([{ "sql_glob": "frozen/*.sql" }]),
            DocMode::Seed,
            &StoreDocContext::new(
                &mutation_root,
                mutation_root.join("vault.yaml"),
                StoreDocScope::Global,
            ),
        )
        .expect("initial glob membership should prepare");
    fs::write(
        frozen_dir.join("20_added_later.sql"),
        "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (41, 'must-not-run');",
    )
    .unwrap();
    mutation_store
        .seed(frozen.doc())
        .await
        .expect("only the frozen glob membership should execute");
    assert_eq!(
        live_rows(&pool).await,
        [(40, "frozen-selected".into())],
        "files created after preparation must not join the active run"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let prefix_one = mutation_root.join("glob-prefix-one");
        let prefix_two = mutation_root.join("glob-prefix-two");
        let prefix_link = mutation_root.join("glob-prefix-link");
        fs::create_dir_all(&prefix_one).unwrap();
        fs::create_dir_all(&prefix_two).unwrap();
        fs::write(
            prefix_one.join("selected.sql"),
            "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (45, 'old-prefix');",
        )
        .unwrap();
        fs::write(
            prefix_two.join("selected.sql"),
            "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (46, 'new-prefix');",
        )
        .unwrap();
        symlink(&prefix_one, &prefix_link).unwrap();
        let prefix_prepared = driver
            .prepare_with_context(
                &json!([{ "sql_glob": "glob-prefix-link/*.sql" }]),
                DocMode::Seed,
                &StoreDocContext::new(
                    &mutation_root,
                    mutation_root.join("vault.yaml"),
                    StoreDocScope::Global,
                ),
            )
            .expect("the original symlinked glob prefix should prepare");
        fs::remove_file(&prefix_link).unwrap();
        symlink(&prefix_two, &prefix_link).unwrap();
        let prefix_error = mutation_store
            .seed(prefix_prepared.doc())
            .await
            .expect_err("retargeting a glob's fixed-prefix symlink must invalidate the plan")
            .to_string();
        for needle in [
            "changed canonical target",
            "glob-prefix-link/selected.sql",
            &prefix_one
                .join("selected.sql")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
            &prefix_two
                .join("selected.sql")
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
        ] {
            assert!(
                prefix_error.contains(needle),
                "expected `{needle}` in:\n{prefix_error}"
            );
        }
        assert_eq!(live_rows(&pool).await, [(40, "frozen-selected".into())]);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let declaration_one = mutation_root.join("declaration-one");
        let declaration_two = mutation_root.join("declaration-two");
        let declaration_link = mutation_root.join("declaration-link");
        fs::create_dir_all(&declaration_one).unwrap();
        fs::create_dir_all(&declaration_two).unwrap();
        fs::write(
            declaration_one.join("relative.sql"),
            "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (47, 'old-declaration');",
        )
        .unwrap();
        fs::write(
            declaration_two.join("relative.sql"),
            "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (48, 'new-declaration');",
        )
        .unwrap();
        symlink(&declaration_one, &declaration_link).unwrap();
        let declaration_prepared = driver
            .prepare_with_context(
                &json!([{ "sql_file": "relative.sql" }]),
                DocMode::Seed,
                &StoreDocContext::new(
                    &mutation_root,
                    declaration_link.join("case.test.yaml"),
                    StoreDocScope::Local,
                ),
            )
            .expect("a relative fixture under the original declaration link should prepare");
        fs::remove_file(&declaration_link).unwrap();
        symlink(&declaration_two, &declaration_link).unwrap();
        let declaration_error = mutation_store
            .seed(declaration_prepared.doc())
            .await
            .expect_err("retargeting the declaring directory must invalidate the frozen plan")
            .to_string();
        let declaration_one_path = declaration_one
            .join("relative.sql")
            .canonicalize()
            .unwrap()
            .display()
            .to_string();
        let declaration_two_path = declaration_two
            .join("relative.sql")
            .canonicalize()
            .unwrap()
            .display()
            .to_string();
        for needle in [
            "changed canonical target",
            "declaration-link/case.test.yaml",
            "local fixture `relative.sql`",
            declaration_one_path.as_str(),
            declaration_two_path.as_str(),
        ] {
            assert!(
                declaration_error.contains(needle),
                "expected `{needle}` in:\n{declaration_error}"
            );
        }
        assert_eq!(live_rows(&pool).await, [(40, "frozen-selected".into())]);
    }

    let external_absolute = mutation_temp.path().join("outside-suite-absolute.sql");
    fs::write(
        &external_absolute,
        "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (50, 'absolute-external');",
    )
    .unwrap();
    let absolute_prepared = driver
        .prepare_with_context(
            &json!([{ "sql_file": external_absolute }]),
            DocMode::Seed,
            &StoreDocContext::new(
                &mutation_root,
                mutation_root.join("vault.yaml"),
                StoreDocScope::Global,
            ),
        )
        .expect("an absolute fixture outside the suite should prepare");
    mutation_store
        .seed(absolute_prepared.doc())
        .await
        .expect("an absolute fixture outside the suite should execute");
    assert_eq!(
        live_rows(&pool).await,
        [
            (40, "frozen-selected".into()),
            (50, "absolute-external".into()),
        ]
    );

    let rollback_global_path = mutation_temp.path().join("global-rollback.sql");
    fs::write(
        &rollback_global_path,
        "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (60, 'global-must-roll-back');",
    )
    .unwrap();
    let local_dir = mutation_root.join("tests");
    fs::create_dir_all(&local_dir).unwrap();
    let invalid_local_path = local_dir.join("invalid-local.sql");
    fs::write(&invalid_local_path, "THIS IS NOT VALID POSTGRES SQL;").unwrap();
    let mut global_prepared = driver
        .prepare_with_context(
            &json!([{ "sql_file": rollback_global_path }]),
            DocMode::Seed,
            &StoreDocContext::new(
                &mutation_root,
                mutation_root.join("vault.yaml"),
                StoreDocScope::Global,
            ),
        )
        .unwrap();
    let local_prepared = driver
        .prepare_with_context(
            &json!([
                {
                    "sql": "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (61, 'local-must-roll-back')"
                },
                { "sql_file": "invalid-local.sql" }
            ]),
            DocMode::Seed,
            &StoreDocContext::new(
                &mutation_root,
                local_dir.join("case.test.yaml"),
                StoreDocScope::Local,
            ),
        )
        .unwrap();
    global_prepared
        .append(local_prepared)
        .expect("distinct global and local fixture plans should combine");
    let combined_error = mutation_store
        .seed(global_prepared.doc())
        .await
        .expect_err("a local SQL error should roll back global and local entries")
        .to_string();
    for needle in [
        "seed.postgres[1].sql_file",
        "local",
        "invalid-local.sql",
        "postgres",
    ] {
        assert!(
            combined_error.contains(needle),
            "expected `{needle}` in:\n{combined_error}"
        );
    }
    assert_eq!(
        live_rows(&pool).await,
        [
            (40, "frozen-selected".into()),
            (50, "absolute-external".into()),
        ],
        "one failed local entry must roll back preceding global and local entries"
    );

    let duplicate_path = mutation_root.join("duplicate-runtime.sql");
    fs::write(
        &duplicate_path,
        "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (65, 'must-not-run-twice');",
    )
    .unwrap();
    let duplicate_global = driver
        .prepare_with_context(
            &json!([{ "sql_file": "duplicate-runtime.sql" }]),
            DocMode::Seed,
            &StoreDocContext::new(
                &mutation_root,
                mutation_root.join("vault.yaml"),
                StoreDocScope::Global,
            ),
        )
        .unwrap();
    let duplicate_local = driver
        .prepare_with_context(
            &json!([{ "sql_file": "../duplicate-runtime.sql" }]),
            DocMode::Seed,
            &StoreDocContext::new(
                &mutation_root,
                local_dir.join("duplicate.test.yaml"),
                StoreDocScope::Local,
            ),
        )
        .unwrap();
    let mut duplicate_entries = duplicate_global.doc().as_array().unwrap().clone();
    duplicate_entries.extend(duplicate_local.doc().as_array().unwrap().iter().cloned());
    let duplicate_error = mutation_store
        .seed(&Value::Array(duplicate_entries))
        .await
        .expect_err("runtime composition must reject a repeated canonical fixture identity")
        .to_string();
    let duplicate_resolved = duplicate_path.canonicalize().unwrap().display().to_string();
    for needle in [
        "already selected",
        "vault.yaml",
        "duplicate.test.yaml",
        duplicate_resolved.as_str(),
    ] {
        assert!(
            duplicate_error.contains(needle),
            "expected `{needle}` in:\n{duplicate_error}"
        );
    }
    assert_eq!(
        live_rows(&pool).await,
        [
            (40, "frozen-selected".into()),
            (50, "absolute-external".into()),
        ],
        "duplicate identity validation must run before either fixture executes"
    );

    let disappearing_path = mutation_root.join("disappearing.sql");
    fs::write(
        &disappearing_path,
        "INSERT INTO vault_sql_file_fixture_cases (id, label) VALUES (70, 'must-not-run');",
    )
    .unwrap();
    let disappearing = driver
        .prepare_with_context(
            &json!([{ "sql_file": "disappearing.sql" }]),
            DocMode::Seed,
            &StoreDocContext::new(
                &mutation_root,
                mutation_root.join("vault.yaml"),
                StoreDocScope::Global,
            ),
        )
        .unwrap();
    let disappearing_resolved = disappearing.files()[0].resolved_path.clone();
    fs::remove_file(&disappearing_path).unwrap();
    let disappearance_error = mutation_store
        .seed(disappearing.doc())
        .await
        .expect_err("a selected fixture removed after preparation must fail")
        .to_string();
    for needle in [
        "seed.postgres[0].sql_file",
        "sql_file `disappearing.sql`",
        &disappearing_resolved.display().to_string(),
        "could not re-resolve SQL file",
    ] {
        assert!(
            disappearance_error.contains(needle),
            "expected `{needle}` in:\n{disappearance_error}"
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let target_one = mutation_root.join("target-one.sql");
        let target_two = mutation_root.join("target-two.sql");
        let link = mutation_root.join("mutable-link.sql");
        fs::write(&target_one, "SELECT 1;").unwrap();
        fs::write(&target_two, "SELECT 2;").unwrap();
        symlink(&target_one, &link).unwrap();
        let target_prepared = driver
            .prepare_with_context(
                &json!([{ "sql_file": "mutable-link.sql" }]),
                DocMode::Seed,
                &StoreDocContext::new(
                    &mutation_root,
                    mutation_root.join("vault.yaml"),
                    StoreDocScope::Global,
                ),
            )
            .unwrap();
        fs::remove_file(&link).unwrap();
        symlink(&target_two, &link).unwrap();
        let target_error = mutation_store
            .seed(target_prepared.doc())
            .await
            .expect_err("a selected file changing canonical target must fail")
            .to_string();
        for needle in [
            "seed.postgres[0].sql_file",
            "changed canonical target",
            &target_one.canonicalize().unwrap().display().to_string(),
            &target_two.canonicalize().unwrap().display().to_string(),
        ] {
            assert!(
                target_error.contains(needle),
                "expected `{needle}` in:\n{target_error}"
            );
        }
    }

    sqlx::query("DROP TABLE vault_sql_file_fixture_cases")
        .execute(&pool)
        .await
        .unwrap();
}
