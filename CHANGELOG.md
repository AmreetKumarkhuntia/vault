## v0.7.0 (2026-10-04)

### Features
- add flow report index and detail pages (b5f8d5f)

## v0.6.0 (2026-10-03)

### Features
- add offline HTML execution reports (1ef044a)

## v0.5.0 (2026-09-30)

### Features
- add YAML-controlled fixture plans (2fb5170)

## Unreleased

### Features
- add suite-global PostgreSQL seed and deterministic `sql_glob` fixture selection
- allow declaration-relative, parent-relative, absolute, and symlinked SQL fixture paths

### Breaking changes
- resolve test-local `sql_file` paths from the declaring test YAML instead of the suite root; nested
  tests may need additional `../` segments

### Security
- treat suite YAML as trusted configuration because SQL selectors may read and execute any
  accessible `.sql` file

## v0.4.0 (2026-09-28)

### Features
- add color-free output mode (ee47eac)
- support validated SQL file fixtures (5960075)

### Other
- plan SQL fixture validation (afb0927)

## v0.3.3 (2026-09-28)

### Fixes
- reject empty runs and report write errors (29fdffe)

### Other
- track Vault shortcomings (be10a77)

## v0.3.2 (2026-09-24)

### Fixes
- publish npm wrapper under thunderkiller scope (9874bb9)

## v0.3.1 (2026-09-24)

### Fixes
- use configured npm auth token (1dee771)

## v0.3.0 (2026-09-24)

### Features
- add testable npx suite runner (9078a6c)

## v0.2.0 (2026-09-23)

### Features
- npx wrapper package @amreetkumarkhuntia/vault (50032bf)
- Makefile targets and cargo aliases for running and testing (7c7ff04)
- demo target, e2e YAML suite, demo script, CI (f956a24)
- vault test harness workspace (0a8c063)

### Other
- release commits on main instead of release-please PRs (98225ee)
- release pipeline — release-please, Linux musl binaries, npm publish (dcbca10)
- lint job and PR-run cancellation (55ba08a)
- apply rustfmt and clippy fixes across the workspace (b565639)
- tests/code for harness tests, tests/flows for YAML suites (91b90df)
- README (92edc20)
- design document for the vault black-box API test harness (a749e8d)
