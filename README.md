# vault

Black-box HTTP API test harness. Point it at a running server and it fires HTTP steps, verifies responses, and can optionally seed and verify Postgres/Redis or impersonate external dependencies with a recording mock — all from declarative YAML, no test code.

Its signature feature is diagnosability: when an expected insertion is missing, the report names exactly which row, and shows a per-column diff against the closest actual row:

```
✗ table `orders`: expected row MISSING — id=1, status=confirmed  (after 2015ms, 20 attempts)
  closest match (score 0.80):
  │ field         expected                     actual                             │
  │ created_at    <within 10s of test start>   2026-09-23T10:55:20.783+00:00 ✓    │
  │ id            1                            1 ✓                                │
  │ status        confirmed                    pending ✗                          │
  │ total_cents   2500                         2500 ✓                             │
```

Missed and unexpected outbound calls get the same treatment: the closest recorded request with component-level diffs.

## How it fits together

```
                  ┌────────────────────────────────────────┐
 YAML suites ───▶ │               vault (CLI)              │
                  │    dsl ─▶ core engine ─▶ report        │
                  └──────┬──────────┬───────────┬──────────┘
                  seeds/ │          │ steps     │ serves canned deps,
                  verify │          │           │ records every request
                         ▼          ▼           ▼
                   ┌──────────┐  ┌────────┐  ┌───────────┐
                   │ Postgres │  │ TARGET │─▶│ mock deps │
                   │  Redis   │◀─│  (your black box)     │
                   └──────────┘  └────────┴───────────────┘
```

The target runs untouched. Its only coupling to vault is configuration: its database DSNs point at the test stores, and its dependency base URLs point at vault's mock listener (`vault env` prints the values to export).

Per-test lifecycle: `RESET → SEED → ARM MOCKS → RUN STEPS → VERIFY END-STATE → REPORT`. Isolation is reset-*before* (TRUNCATE / FLUSHDB), so a crashed run never poisons the next one and failed-test state stays in place for post-mortem.

## Install

```sh
# Install in a project, then use the short executable name:
npm install --save-dev @thunderkiller/vault
npx vault --run ./tests/flows

# Or run the scoped package without installing it first:
npx @thunderkiller/vault --help

# prebuilt binary: grab the tar.gz for your arch from the Releases page
#   https://github.com/AmreetKumarkhuntia/vault/releases  (verify with SHA256SUMS)

# from source (any platform, incl. macOS):
cargo install --git https://github.com/AmreetKumarkhuntia/vault vault
```

## Quick start

```sh
# backing stores: local Postgres + Redis, or `make stores-up` (docker, ports 5433/6380)
make demo        # guided demo: suite, safety checks, and failure showcase
```

Day-to-day commands (`make help` lists all):

```sh
make test        # workspace Rust tests, including black-box CLI subprocess tests
make suite       # start demo-target, run the YAML flow suite, stop it
make suite ARGS='-t smoke -v'
make negative    # the deliberately-failing showcase (exit 1 is the point)
make test-all    # test + suite + negative
make validate    # static-check every YAML file
make target-start / target-stop   # manage demo-target yourself
```

Cargo aliases work too once the target is up: `cargo suite`, `cargo suite-list`, `cargo suite-validate`, `cargo suite-env`.

Manual flow against your own server:

```sh
cargo build --workspace
vault env                  # print the env the target should start with
# start your target with those URLs …
vault validate             # static-check every YAML file, no execution
vault list                 # resolved run plan
vault list --fixtures      # ordered SQL fixture plans, no database connection
vault run                  # everything: flows + standalone tests
vault run 'create order*'  # one test by name/glob
vault run -t smoke         # filter by tag
vault run --step           # pause at each lifecycle boundary: resp / vars / calls / db <sql> / redis <cmd>
vault run --shuffle        # order-independence audit
```

The npm-friendly shorthand accepts either the suite directory or its root config file, and forwards additional run options:

```sh
npx vault --run ./tests/flows
npx vault --run ./tests/flows/vault.yaml -t smoke
```

### Color-free output

Vault automatically uses color only for capable terminal streams and strips ANSI escapes when
stdout or stderr is redirected. Use the global `--no-color` flag before or after a subcommand for
deterministic plain output:

```sh
vault --no-color run --suite-dir tests/flows
npx vault --no-color --run tests/flows
```

A non-empty `NO_COLOR` or `CLICOLOR=0` also disables color. These settings, in that order, take
priority over `CLICOLOR_FORCE`; an empty `NO_COLOR` is treated as unset. Unicode status glyphs and
table borders remain, while JSON and JUnit artifacts are always unstyled.

To exercise the packed npm CLI from this checkout before a release exists:

```sh
make npm-smoke
```

Exit codes: `0` pass · `1` a test failed · `2` config/usage error · `3` environment/preflight/report/harness error.

An empty selection is an error for `vault run`, not a vacuous pass. If a pattern
and/or tags match no tests or flows, Vault prints the requested pattern and tags
and exits `2` before creating the async runtime, connecting stores, starting the
mock server, running preflight, or writing reports. The equivalent `vault list`
query remains an inspection command: it prints `0 flows, 0 standalone tests` and
exits `0`.

After a non-empty run, Vault attempts every requested HTML, JSON, and JUnit report write.
A failed write does not prevent the other requested formats from being written.
Any report-write failure is printed with its format and path and
makes the command exit `3`; successfully written sibling artifacts are retained.
`make demo` exercises both safeguards, including report writes from an isolated
temporary working directory.

### HTML reports

Generate reports for the current run and open `target/index.html` in a browser:

```sh
vault run --html target/vault-report.html
npx vault --run ./tests/flows --html target/vault-report.html
```

The demo suite enables this output in `vault.yaml`. Your own suite can do the same:

```yaml
report:
  html: target/vault-report.html
  json: target/vault-report.json
  junit: target/vault-junit.xml
  redact:
    headers: [X-Internal-Token]
    fields: [customer_secret]
    json_paths: ["$.customer.email"]
    text_patterns: ['account-secret-[A-Za-z0-9]+']
```

`--html` overrides the configured HTML path. Relative output paths use the process working
directory, just like JSON and JUnit. Vault already discovers and executes all selected flows
in one invocation; no shell loop over flows is needed. HTML output includes:

```text
target/
├── index.html                  # Names, statuses and links for this invocation
├── vault-report.html           # Full aggregate report at the requested path
└── vault-report-pages/
    ├── style.css               # Shared stylesheet for every generated page
    ├── flow-001.html
    ├── flow-002.html
    └── test-003.html            # Selected standalone tests also get a page
```

The index follows execution order and includes failed, skipped and unexecuted items. Detail
pages show only that flow or standalone test and link back to the index. File numbers follow
the selected item order, so repeated stage names remain distinct within their flow.

Each successful invocation replaces the index and Vault-owned detail pages and stylesheet in
that directory; it does not accumulate separate runs. Use separate output directories to retain
multiple run bundles. A small ownership file inside `vault-report-pages/` tracks generated
filenames for cleanup, not report history. Unrelated files are preserved, and conflicting unowned
index, detail, or stylesheet files cause a report-write error. If `--html` names `index.html`
itself, that file contains both the flow links and the full aggregate report.

Copy or publish the whole directory, including `vault-report-pages/style.css`, to keep every
page styled and its relative links working. The bundle works offline; no external scripts,
fonts, or services are needed. The Rust `vault_report::to_html()` API still returns a standalone
HTML document with embedded CSS for callers that need a single file.
Search and status filters narrow the visible tests while totals remain visible; flow stages
link to their test details. Failed tests expand automatically, and light/dark themes and print
styles are included. JSON and JUnit retain their configured paths and aggregate formats.

For Jenkins, generate into a dedicated directory (for example,
`vault run --html target/vault-reports/report.html`) and publish the entire bundle with the
[HTML Publisher plugin](https://www.jenkins.io/doc/pipeline/steps/htmlpublisher/):

```groovy
publishHTML(target: [
    reportDir: 'target/vault-reports',
    reportFiles: 'index.html',
    reportName: 'Vault reports',
    includes: '**/*',
    keepAll: true,
    alwaysLinkToLastBuild: true,
    allowMissing: false
])
```

Put this step in `post { always { ... } }` to publish reports even when tests fail. The shared
stylesheet supports Jenkins' default policy for CSS served from the same origin. Jenkins'
[default content security policy](https://www.jenkins.io/doc/book/security/configuring-content-security-policy/)
still blocks JavaScript, which the aggregate and detail pages need to display results and
interactive controls. The separate index table remains readable. To explore the complete report,
download the whole bundle and open it locally, or use an administrator-configured
[Resource Root URL](https://www.jenkins.io/doc/book/security/user-content/).

When HTML is requested, Vault records the actual lifecycle: setup, store resets and seeds,
watch snapshots, mock setup, prepared HTTP requests and responses, retries, captures, verification
polls, and outbound calls. Earlier evidence survives errors and timeouts; later actions are marked
as not run. Reset-once flows show state reuse, and repeated test names have distinct stage identities.
The report shows observed test evidence, not internal target execution or code coverage. Validation,
empty-selection, and preflight errors occur before a run report exists.

Standard credential headers and common secret fields are masked in terminal output and all
report formats after matching and flow exports complete. `report.redact` adds header names, field
names (including query parameters and captures), JSONPath selectors, and regular expressions for
text payloads and diagnostics. These additions extend the built-in rules. The original values
remain available to matchers in memory. HTML payload previews are capped at 64 KiB after masking
and carry a truncation label. The JSON v1 schema and JUnit structure are unchanged.

CI retains separate normal-suite and negative-showcase HTML, JSON, and JUnit reports in the
`vault-test-reports` artifact for 14 days.

## HTTP-only suites (no Postgres or Redis)

State stores are optional. Leave `postgres` and `redis` out of `vault.yaml`, and omit their `seed` / `verify` blocks from tests. Vault will not connect to, reset, or verify either service:

```yaml
# vault.yaml
version: 1
environments:
  local:
    target:
      base_url: http://127.0.0.1:8080
      health_check: { path: /healthz, timeout: 5s }
    mock_server:
      bind: 127.0.0.1:0
```

Place HTTP tests in nested `*.test.yaml` files as usual. See `tests/http-only` for a runnable example.

## Anatomy of a test

```yaml
# tests/flows/orders/create_order.test.yaml
test: create order happy path
seed:
  postgres:
    - table: users
      rows: [{ id: 1, email: alice@example.com, status: active }]
  redis:
    - set: { key: "session:alice", value: tok_abc, ttl: 600 }
mocks:
  payments:                       # target's PAYMENTS_URL → http://<mock>/payments
    unmatched: fail               # any request no stub matches serves 599 and fails the test
    stubs:
      - name: charge-ok
        match: { method: POST, path: /v1/charges, body: { json_partial: { currency: USD } } }
        response: { status: 201, json: { id: "ch_{{ uuid() }}", status: succeeded } }
steps:
  - name: create
    request: { method: POST, path: /api/orders, json: { user_id: 1, items: [...] } }
    expect:  { status: 201, json_partial: { status: pending } }
    capture: { order_id: { jsonpath: $.id } }       # JSON-typed: numbers stay numbers
verify:                           # end-state, evaluated after all steps
  postgres:
    - table: orders
      expect:
        - id: "{{ order_id }}"
          status: pending
          external_charge_id: { regex: "^ch_" }     # matchers: regex, gt/gte/lt/lte, one_of …
          created_at: !near-now 10s                 # tags: !any !uuid !null !not-null !iso8601 …
      count: exact                # extra rows in this key-space fail the test
      eventually: 5s              # bounded polling for async writers (+ settle for negatives)
  redis:
    - key: "order:{{ order_id }}:summary"
      json_partial: { status: pending }
      ttl: { gt: 0 }
  calls:                          # what the TARGET sent to its dependencies
    - name: exactly one charge
      dependency: payments
      match: { method: POST, path: /v1/charges }
      body: { json_partial: { order_ref: "{{ order_id }}" } }
      count: 1                    # also {gte: n}, {lte: n}; 0 = "never called"
  ordered: [[exactly one charge, confirmation email]]
  unexpected: fail                # unplanned traffic to mocked deps fails the test
```

### PostgreSQL SQL fixtures

SQL fixture selection is entirely YAML-controlled. Put suite-global fixtures in the root
`vault.yaml`; Vault reapplies them after every database reset boundary:

```yaml
# tests/flows/vault.yaml
seed:
  postgres:
    - sql_file: fixtures/global/00_schema.sql
    - sql_glob: "fixtures/global/*.seed.sql"
```

Put scenario-specific fixtures in a test file. Paths are relative to the YAML file that declares
them, so a nested test can refer to files inside or outside the suite:

```yaml
# tests/flows/orders/create_order.test.yaml
seed:
  postgres:
    - sql_file: ../fixtures/order_base.sql
    - sql_glob: "../fixtures/order_states/*.sql"
    - sql_file: ../../shared-fixtures/customer.sql
    - sql_glob: "${env.SQL_FIXTURE_ROOT}/orders/**/*.sql"
    - sql: "UPDATE orders SET status = 'fixture-ready' WHERE id = 7101;"
```

`sql_file` selects exactly one literal file; wildcard characters in its value are not expanded.
`sql_glob` selects one or more files, must contain `*`, `?`, or `**`, and fails validation when it
matches nothing. `*` and `?` stay within one path segment, while `**` crosses directories. Matches
are sorted by their normalized logical paths and expanded at the selector's position, so names such
as `10_users.sql` and `20_orders.sql` make ordering explicit. Selecting the same canonical file
twice—directly, through overlapping globs, or through a symlink—is an error. Vault never discovers
SQL implicitly; a bare directory is not a selector. `**` must be a complete path segment, and any
parent (`..`) components in a glob must appear before its first wildcard.

Root selectors resolve from the directory containing `vault.yaml`; test selectors resolve from the
directory containing that `*.test.yaml`. `../` and `../../` have no suite-boundary restriction, and
Unix absolute paths, Windows drive paths, and UNC paths are accepted. Paths do not depend on the
process working directory. `~` and shell expressions are not expanded; use `${env.NAME}` when an
absolute location differs by machine. Globs use `/` separators on every platform, and Vault applies
no fixture-root allowlist. Absolute forms must be native to the current operating system; a foreign
Windows or Unix form is diagnosed rather than reinterpreted as a local path. This
declaration-relative rule changes earlier `sql_file` behavior: a
nested test that previously used
`fixtures/base.sql` for a suite-root file should use `../fixtures/base.sql` (or the appropriate
number of parent segments).

Vault expands and validates the complete selection before opening database, target, or mock-server
connections, then freezes that ordered file list. Immediately before execution it resolves and
reads each frozen file again; a removed file, changed symlink target, unreadable file, invalid
UTF-8, directory, or non-lowercase-`.sql` match fails the seed. Directory symlinks discovered while
walking a glob are not followed, while exact paths and a glob's fixed prefix may traverse symlinks.

For each reset boundary, suite-global entries run first, followed by the current test's entries.
File SQL, inline SQL, and structured rows execute in declaration order in one PostgreSQL
transaction, and any read, validation, or execution error rolls back the entire effective seed.
With `flow reset: each`, global fixtures run for every executing stage; with `reset: once`, they run
once before the first stage that enters its lifecycle and later stages add only their local seeds.
Skipped tests and empty selections run no fixtures. For `reset: once` plus `on_failure: continue`,
`vault list --fixtures` marks later global plans as conditional because a pre-lifecycle failure can
leave initialization pending for the next stage.

Files must contain UTF-8 PostgreSQL SQL. Their contents are not templated, and `psql` meta-commands
are not interpreted. Top-level transaction-control statements are rejected so fixtures cannot
commit, roll back, or otherwise take ownership of Vault's transaction. Use `vault list --fixtures`
to inspect each scope, selector, logical path, resolved absolute path, execution order, and any
conditional reset-bound branch without connecting to PostgreSQL.

Fixture YAML is trusted configuration: an exact path or glob may read and execute any accessible
`.sql` file. Do not run untrusted suites or pull requests with sensitive filesystem access or
database credentials. Global DDL should be idempotent because truncate-based reset modes preserve
tables and reapply global fixtures. See the runnable exact/glob/parent-relative example in
`tests/flows/orders/sql_file_seed.test.yaml`.

### Flows: use one test's data in the next

```yaml
# tests/flows/orders/lifecycle.flow.yaml
flow: order lifecycle
reset: once                  # the flow is the isolation unit
stages:
  - test: create order happy path
    export: [order_id]       # promote captures into flow scope
  - test: update order status
    with: { order_id: "{{ flow.order_id }}" }
  - test: delete order
    with: { order_id: "{{ flow.order_id }}" }
on_failure: skip-rest        # a failed stage marks the rest SKIPPED, never silent green
```

Referenced tests stay independently runnable (their `vars:` provide defaults; `with:` overrides). A `{{ flow.* }}` reference no earlier stage exports is a static validation error.

## Repository layout

```
crates/
  dsl/             YAML schema, parsing (!tag matchers), static validation
  store/           StateStore trait boundary, outcome types, shared matcher vocabulary
  store-postgres/  sqlx driver: seed / TRUNCATE reset / near-miss verification / watch
  store-redis/     redis driver: seed / FLUSHDB reset / typed key verification
  mock/            recording mock server + outbound-call verification (bipartite matching)
  core/            engine: lifecycle, templating (minijinja), captures, eventually/settle, flows
  report/          terminal / JSON / JUnit / offline HTML renderers and masking
  cli/             the `vault` binary + black-box CLI tests in cli/tests
examples/
  demo-target/     the order service the e2e suite runs against
tests/
  code/            Rust tests for the harness code (`cargo test --workspace`)
  flows/           the YAML suites that test flows: vault.yaml config,
                   *.test.yaml tests, *.flow.yaml flows, fixtures/
docs/DESIGN.md     full design document
```

`vault run` reads `tests/flows` by default (`--suite-dir` overrides). `vault --run <path>` is a shorthand for the same command and also accepts the path to `vault.yaml`. The store layer is a trait boundary: adding MySQL or Mongo later is a new driver crate plus one registration line in the CLI — the engine never learns store specifics, because seed/verify blocks are driver-owned documents.

## Notes & limitations (v1)

The prioritized, evidence-backed backlog is tracked in
[`docs/SHORTCOMINGS.md`](docs/SHORTCOMINGS.md). It separates Vault limitations from adopter-specific
CI, coverage, and application behavior.

- Execution is serial by design: one target, one database. The session abstractions reserve hooks for parallel lanes.
- Transaction-rollback isolation is impossible for a black-box target (it owns its own connections) — that's why isolation is TRUNCATE-based and documented as such.
- When enabled, Redis verification requires a dedicated logical DB (e.g. `/15`); the driver refuses db 0 without `allow_db0: true`.
- The target's in-process caches don't reset with the DB; if your server has a reset endpoint, call it via a seed `sql:`-style hook or an extra step.
- `skip: "${env.FLAG:-reason}"` gates a test on an environment variable — an empty value means "run it" (see `tests/negative/`).
