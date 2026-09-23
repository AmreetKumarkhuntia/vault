# `testkit` — Black-Box HTTP API Test Harness: Final Design

> Implementation note: the project shipped under the name **vault** (crates `vault-*`, binary `vault`, config `vault.yaml`). Read `testkit` below as `vault`.

**Status:** Recommended design (synthesis of Designs A/B/C) · **Language:** Rust (tokio) · **Deliverable:** design only, no implementation yet

## Conflict resolutions (summary)

| Topic | Decision | Why (one line) |
|---|---|---|
| YAML crate | `serde_yaml_ng` (B) | `serde_yaml` is archived; pin a maintained API-identical fork. |
| JSON diffing | Own ~200-line structured `jsondiff` module, drop `assert-json-diff` (C over A/B) | Reports need machine-readable `FieldDiff`s; assert-json-diff's output is a string and panic-oriented. |
| End-state block keyword | `verify:` at test level, `expect:` per step (B/C over A) | Two different scopes deserve two words; avoids overloading one keyword. |
| Store trait shape | B's registry + driver-owned opaque docs, flattened into one `StateStore` facade | Every driver implements all roles anyway; the registry/driver split is what buys "MySQL = one new crate". |
| Outbound-call matching | C's bipartite-assignment algorithm | Greedy matching gives false negatives when expectations overlap; N is tiny so exact matching is free. |
| Polling | C's `eventually:` (with settle semantics) + A's step-level `repeat:` | Settle semantics are the only correct treatment of negative assertions; step `repeat:` covers HTTP-level polling. |
| Unmatched mock response | Serve `599` (A over B's 404 / C's 501) | Non-standard code is unmistakable in target logs and can't be swallowed by target error handling as "normal". |
| Exit codes | C's `0/1/2/3/130` | Distinguishing config errors (2) from infra errors (3) is what CI dashboards actually need. |
| Count syntax | A's numeric/operator-map form (`count: 1`, `count: {gte: 1}`) | Consistent with every other matcher in the DSL; no mini-grammar (`exactly 1`) to parse. |
| Unexpected-change detection | C's `watch:` mode (PK + row-hash snapshot, claim semantics) over A's `unchanged:`/B's `guard:` | Only C's design forces every observed diff to be *explained* by an expectation, which is the actual guarantee users want. |
| Global config | `testkit.yaml` (A's format) loaded through figment layering (B's mechanism) | Test authors live in YAML — one syntax; figment still gives flags > env > file precedence. |
| Matcher wildcards | A's YAML-tag vocabulary (`!any`, `!uuid`, `!iso8601`, `!near-now`, …) shared across HTTP/DB/Redis/outbound | One vocabulary to learn; tags survive serde cleanly. |
| Interactive step mode | C's command set on B's TTY-free `Gate` abstraction | Rich inspection UX without the engine knowing about terminals. |

---

## 1. Overview & end-to-end flow

`testkit` is a CLI binary that treats the target HTTP server as a sealed black box and controls everything around it: it seeds Postgres/Redis, impersonates the target's external dependencies with an embedded recording mock server, fires HTTP steps at the target, and verifies responses, database end-state, Redis end-state, and outbound calls — producing diagnosable, structured failure reports (especially: *which* expected row is missing, with near-miss diffs).

```
                    ┌────────────────────────────────────────┐
   YAML suites ───▶ │              testkit (CLI)             │
                    │   dsl ─▶ core engine ─▶ report          │
                    └──────┬──────────┬───────────┬──────────┘
                    seeds/ │          │ steps     │ serves canned deps,
                    verify │          │ (reqwest) │ records every request
                           ▼          ▼           ▼
                     ┌──────────┐  ┌────────┐  ┌───────────┐
                     │ Postgres │  │ TARGET │─▶│ mock deps │
                     │  Redis   │◀─│ (black box HTTP srv)  │
                     └──────────┘  └────────┴──────────────┘
```

The target is started separately; its only coupling to testkit is configuration: its DB/Redis DSNs point at the test stores, and its dependency base URLs point at testkit's mock listener (`testkit env` prints the values to export).

### Per-test lifecycle (state machine)

```
LOAD ─▶ VALIDATE ─▶ RESET ─▶ SEED ─▶ ARM_MOCKS ─▶ RUN_STEPS ─▶ VERIFY_END_STATE ─▶ REPORT ─▶ CLEANUP
```

| Stage | What happens | On failure |
|---|---|---|
| LOAD / VALIDATE | Parse all matched YAML; static cross-checks (template refs, step-name ordering, mock names, fixture paths, `deny_unknown_fields` typo detection). | Whole run aborts, exit 2, before any test executes. |
| RESET | Isolation strategy: `TRUNCATE ... RESTART IDENTITY CASCADE`, `FLUSHDB`, clear mock stubs/recordings. Runs **before** each test. | Test `ERRORED`, skip to REPORT; later tests still run. |
| SEED | Insert PG rows (one transaction), set Redis keys. | `ERRORED`, fail fast. |
| ARM_MOCKS | Install stub table into the mock hub; recording buffer starts empty. | `ERRORED`. |
| RUN_STEPS | Per step: render templates → fire request → capture variables → evaluate inline `expect:`. Fail-fast **between** steps (captures downstream would be garbage); collect-all **within** a step. | Assertion failure = `FAILED`; transport/render error = `ERRORED`. |
| VERIFY_END_STATE | Evaluate `verify:` — postgres, redis, outbound calls. **Always collect-all**, and runs **even if steps FAILED** (not if ERRORED): "response was wrong *and* here's what hit the DB/mocks" is the debugging gold. | `FAILED`. |
| REPORT | Emit structured result; stream terminal output. | — |
| CLEANUP | Disarm mocks (via `Drop` guard — survives panic/ctrl-C), release per-test resources. State is **left in place** on failure for post-mortem; the next test's RESET makes it irrelevant. | Warnings only. |

**Run-level preflight:** before test 1, probe target health endpoint, Postgres, Redis, and bind the mock listener. Any probe failure → exit 3 immediately with a reachability table. Never let 40 tests each time out against a target that isn't running.

**Status lattice:** `PASSED < FAILED < ERRORED` per test; run status = worst. Execution is **serial in v1** (one target, one DB — see risks); the session/registry abstractions reserve the hooks for future parallelism.

---

## 2. YAML DSL spec

### 2.1 File & suite organization

```
tests/
  testkit.yaml              # global config (one per project root)
  fixtures/                 # seed fragments, mock fragments, .sql files (not discovered as tests)
  orders/
    _suite.yaml             # optional per-directory defaults (tags, shared seeds/mocks, timeouts)
    create_order.test.yaml  # one test per file (recommended); a file MAY hold `tests: [...]`
```

### 2.2 Global config: `testkit.yaml`

```yaml
version: 1
environments:
  local:
    target:
      base_url: http://localhost:8080
      health_check: { path: /healthz, timeout: 30s }
    postgres: { url: "${env.TESTKIT_PG_URL:-postgres://test:test@localhost:5433/app_test}" }
    redis:    { url: "${env.TESTKIT_REDIS_URL:-redis://localhost:6380/15}" }   # dedicated logical DB
    mock_server: { bind: 127.0.0.1:0 }        # 0 = ephemeral; `testkit env` prints URLs to export
  ci:
    mock_server: { bind: 0.0.0.0:9099 }       # fixed port when the target's env is baked at container start
defaults:
  request: { timeout: 10s, headers: { Content-Type: application/json } }
  mock:    { unmatched: fail }                # fail | respond: {status: 404} | passthrough: <url>
  reset:
    postgres: { mode: truncate, exclude: [schema_migrations] }   # truncate | script:<path> | none
    redis:    { mode: flushdb }                                  # flushdb | scan: {prefixes: [...]}
  verify:  { settle: 500ms }
report: { json: target/testkit-report.json, junit: target/testkit-junit.xml }
```

Only `${env.VAR}` / `${env.VAR:-default}` interpolation is allowed here (config-time); the `{{ }}` engine is test-time only. DSNs and the mock bind address are global-config-only — a suite file can never silently point at prod. Layering (figment): CLI flags > `TESTKIT_*` env > `testkit.yaml` > defaults.

### 2.3 Test file shape

```yaml
test: <unique name>          # required
description: ...             # optional
tags: [orders, smoke]
skip: false                  # or a string reason
timeout: 60s                 # whole-test budget
vars: {}                     # test-local constants → {{ vars.* }}
watch: [orders]              # opt-in unexpected-change detection (§4.4)
seed:    { postgres: [...], redis: [...] }
mocks:   { <dep-name>: {...} }
steps:   [ ... ]
verify:  { postgres: [...], redis: [...], calls: [...] }
```

### 2.4 Seeding

**Postgres** — entries executed in order, one transaction; structured rows preferred (the runner then knows exactly what it inserted, powering receipts and error messages):

```yaml
seed:
  postgres:
    - sql_file: fixtures/big_catalog.sql          # raw SQL escape hatch
    - sql: "ALTER SEQUENCE orders_id_seq RESTART WITH 1000;"
    - table: users
      conflict: error                             # error (default) | ignore | update
      rows:
        - { id: 1, email: alice@example.com, status: active, created_at: "{{ now() }}" }
    - fixture: fixtures/users.seed.yaml           # include a fragment (list of these entries)
```

`null` → SQL NULL; nested maps/lists → json/jsonb; `!base64` for bytea. Templating applies (`{{ uuid() }}`, `{{ vars.* }}`) but **step captures are statically rejected in seeds** (seeds run before steps).

**Redis** — one entry = one key:

```yaml
seed:
  redis:
    - set:  { key: "session:alice", value: tok_abc, ttl: 300 }
    - set:  { key: "cfg:limits", json: { max_orders: 10 } }     # JSON-serialized string
    - hash: { key: "user:1", fields: { name: alice } }
    - list: { key: "queue:emails", values: [a, b] }             # RPUSH order
    - zset: { key: "leaders", members: { alice: 100 } }
    - sadd: { key: "flags:beta", members: [u1, u2] }
```

### 2.5 Mocks

One embedded listener serves N dependencies via stable path prefixes (`{{ mock.payments.base_url }}` = `http://<bind>/payments`); `dedicated_port: true` binds an extra listener for targets that can't tolerate prefixes.

```yaml
mocks:
  payments:
    prefix: /payments            # default /<name>
    unmatched: fail              # per-dep override; serves 599 + JSON error, fails the test
    stubs:
      - name: charge-ok          # names make reports readable
        match:
          method: POST
          path: /v1/charges      # exact | "/v1/charges/{id}" (params captured) | {regex: "..."} | trailing /**
          query:   { idempotent: "true" }              # subset; "*" = key present
          headers: { Authorization: "Bearer {{ vars.KEY }}" }   # subset, case-insensitive names
          body:    { json_partial: { currency: USD } } # also: json_exact | regex | jsonpath: {path,eq}
        response:
          status: 201
          headers: { Content-Type: application/json }
          json: { id: "ch_{{ uuid() }}", status: succeeded }    # templated per request;
          latency: 30ms                                         # {{ request.path_params.* }} etc. available
        times: 2                 # optional serving budget; exhausted stub stops matching
      - name: refund-then-fail
        match: { method: POST, path: /v1/refunds }
        responses:               # plural = sequence, cursor per test
          - { status: 201, json: { status: pending } }
          - { status: 500, json: { error: upstream_down } }
          - repeat: last         # last | cycle | then: unmatched
```

**Matching semantics (fixed):** stubs tried in declaration order, first full match wins — no specificity scoring, predictability beats cleverness. Every request is **recorded regardless of match outcome** (method, URL, headers, raw+parsed body, timestamp, matched-stub or UNMATCHED, global monotonic `seq`); the recording is the single source of truth for `verify.calls` and `unmatched: fail`.

### 2.6 Steps

```yaml
steps:
  - name: create order                    # required, unique per test
    request:
      method: POST                        # GET | POST | PUT | DELETE | PATCH
      path: /api/orders                   # joined to target.base_url; `url:` for absolute
      query: { dry_run: "false" }
      headers: { Authorization: "Bearer {{ steps.login.captures.token }}" }
      json: { user_id: 1 }                # json: | body: (raw) | form: (urlencoded) | file: <path>
      timeout: 5s
    expect:                               # inline response assertions (collect-all within step)
      status: 201                         # exact | 2xx | [200, 201]
      headers: { Location: { regex: "^/api/orders/\\d+$" } }   # subset; also `absent`
      json_partial: { status: pending }   # containment; `unordered: true` for arrays
      json_exact:  { ... }                # deep equality; sibling `ignore: [$.created_at]`
      jsonpath:
        - { path: $.total_cents, gt: 0 }  # ops: eq ne gt gte lt lte regex exists absent len contains one_of type
      body: { regex: "..." }              # non-JSON bodies
      time: { under: 500ms }
    capture:
      order_id:  { jsonpath: $.id }       # jsonpath | header: Name | status: true | body: text
      order_url: { header: Location }     #   | regex: {on: body, pattern: "...", group: 1}
    repeat: { every: 250ms, timeout: 10s }  # optional: re-run step until expect passes (HTTP polling)
```

Captures are JSON-typed (a number stays a number). Namespaces: `{{ steps.<name>.captures.<var> }}` fully qualified, plus a flat `{{ <var> }}` shorthand (lint warns on shadowing).

### 2.7 End-state `verify:`

All three blocks are **named lists of structured expectations** — never scripts — so the runner can enumerate exactly what is missing and compute near-misses.

```yaml
verify:
  postgres:
    - table: orders
      key: [user_id, sku]                 # identity cols (optional; else all literal cols)
      expect:                             # each entry = one expected row
        - user_id: 1
          status: confirmed
          total_cents: { gt: 0 }
          external_charge_id: { regex: "^ch_" }
          id: !uuid                       # tag wildcards: !any !null !not-null !uuid !iso8601
          created_at: !near-now 5s        #   !number !near-now <tol> !json <partial> !one-of [..] !absent
      count: exact                        # exact (extras in key-space = UNEXPECTED_ROW) | at_least
      eventually: 5s                      # §4.3 polling
    - table: audit_log
      expect_absent:                      # absence assertion (settle semantics under eventually)
        - { entity_id: "{{ order_id }}" }
  redis:
    - key: "order:{{ order_id }}:summary"
      type: string
      json_partial: { status: pending }   # or value: exact | regex: | absent: true
      ttl: { gt: 0, lte: 3600 }
    - pattern: "lock:*"                   # cursor SCAN, never KEYS
      count: 0
      eventually: 2s
  calls:
    - name: exactly one charge            # label for reports
      dependency: payments
      match: { method: POST, path: /v1/charges }
      body: { json_partial: { amount_cents: 2500, order_ref: "{{ order_id }}" } }
      count: 1                            # or {gte: 1}, {lte: 2}, 0 = "never called"
      dedupe: { jsonpath: $.idempotency_key }   # collapse target retries before counting
  ordered:                                # optional order groups (subsequence semantics)
    - [exactly one charge, email sent]
  unexpected: fail                        # fail (default for mocked deps) | allow | allow_for: [metrics]
```

### 2.8 Templating

- Engine: **minijinja**, `{{ }}` only — `{% %}` logic blocks rejected in v1 (declarative tests, not programs).
- Namespaces: `env.*` (allow-listed process env), `vars.*`, `steps.<name>.captures.*` + flat capture shorthand, `mock.<dep>.base_url`, `request.*` (inside mock responses), `test.name`, `run.id`.
- Builtins: `uuid()`, `now()` / `now('+2h')`, `random_int(a,b)`, filters `b64encode`, `sha256`, `int`, `json`. `now()` is **frozen per test** at SEED so `!near-now` compares against a stable anchor.
- **Typing rule:** a scalar that is exactly one expression (`id: "{{ order_id }}"`) substitutes the native JSON value (number stays number — critical for DB matchers); mixed strings stringify.
- Render timing: config at load; each step just before firing; `verify:` after all steps. Templates never apply to structural keys (table names, step names), keeping static validation possible.

### 2.9 Reuse

YAML anchors (native, within file) · `fixture: <path>` list items in seeds/mocks · `{$include: path, with: {overrides}}` deep-merge for any mapping · `_suite.yaml` defaults (lists prepend, scalars are overridden). No test inheritance or `matrix:` in v1 (key reserved).

### 2.10 Full example

```yaml
# tests/orders/create_order.test.yaml
test: create order happy path
description: POST /orders charges card via payment-service, inserts order row, warms cache
tags: [orders, smoke]
timeout: 60s

seed:
  postgres:
    - table: users
      rows: [{ id: 1, email: alice@example.com, status: active, plan: pro }]
    - table: wallets
      rows: [{ user_id: 1, balance_cents: 50000, currency: USD }]
  redis:
    - set: { key: "session:alice", value: tok_abc, ttl: 600 }

mocks:
  payments:
    unmatched: fail
    stubs:
      - name: charge-ok
        match:
          method: POST
          path: /v1/charges
          body: { json_partial: { currency: USD } }
        response:
          status: 201
          json: { id: "ch_{{ uuid() }}", status: succeeded }
          latency: 30ms
  email:
    stubs:
      - name: send-ok
        match: { method: POST, path: /send }
        response: { status: 202, body: "" }

steps:
  - name: create
    request:
      method: POST
      path: /api/orders
      headers: { Authorization: Bearer tok_abc }
      json: { user_id: 1, items: [{ sku: SKU-BOOK, qty: 1, price_cents: 2500 }] }
    expect:
      status: 201
      json_partial: { status: pending, user_id: 1 }
      jsonpath: [{ path: $.id, type: number }]
    capture:
      order_id: { jsonpath: $.id }

  - name: order visible
    request: { method: GET, path: "/api/orders/{{ order_id }}" }
    expect: { status: 200, json_partial: { id: "{{ order_id }}", total_cents: 2500 } }
    repeat: { every: 200ms, timeout: 5s }

verify:
  postgres:
    - table: orders
      expect:
        - id: "{{ order_id }}"
          user_id: 1
          status: pending
          total_cents: 2500
          external_charge_id: { regex: "^ch_" }
          created_at: !near-now 10s
      count: exact
      eventually: 5s
    - table: wallets
      expect: [{ user_id: 1, balance_cents: 47500 }]
  redis:
    - key: "order:{{ order_id }}:summary"
      json_partial: { status: pending, total_cents: 2500 }
      ttl: { gt: 0 }
  calls:
    - name: exactly one charge
      dependency: payments
      match: { method: POST, path: /v1/charges }
      body: { json_partial: { amount_cents: 2500, currency: USD, order_ref: "{{ order_id }}" } }
      count: 1
    - name: confirmation email
      dependency: email
      match: { method: POST, path: /send }
      count: 1
  ordered:
    - [exactly one charge, confirmation email]
  unexpected: fail
```

### 2.11 Flows — cross-test data & sequencing

Two different chaining scopes, two mechanisms:

1. **Within a test** — step captures (§2.6): `{{ steps.create.captures.order_id }}` / `{{ order_id }}`. Already covered.
2. **Across tests** — **flows**: an ordered chain of existing tests sharing state and variables, defined in a `*.flow.yaml` file. This is how "use the previous test's data in the next one" works.

```yaml
# tests/orders/lifecycle.flow.yaml
flow: order lifecycle
tags: [orders]
reset: once                    # once (default): RESET runs before stage 1 only — the flow
                               # is the isolation unit; each: normal per-test isolation
stages:
  - test: create order happy path      # references a test by name
    export: [order_id]                 # promote this test's captures into flow scope
  - test: update order
    with:                              # inject into the test's vars (overrides its defaults)
      order_id: "{{ flow.order_id }}"
  - test: delete order
    with: { order_id: "{{ flow.order_id }}" }
on_failure: skip-rest          # remaining stages report SKIPPED(dependency failed); also: continue
```

**Semantics:**

- **Referenced tests stay independently runnable.** A test consumed by a flow declares its inputs as `vars:` with defaults (or marks them `required: true`, in which case running it standalone without `--var` is a validation error). `with:` overrides those vars; nothing inside the test file knows about flows.
- **Exports are explicit.** `export: [order_id]` promotes a stage's captures to `{{ flow.order_id }}`; the fully-qualified `{{ flow.stages.<stage>.captures.* }}` namespace always exists. Values stay JSON-typed end to end. VALIDATE statically rejects a `{{ flow.* }}` reference that no earlier stage exports.
- **Isolation:** with `reset: once`, RESET (TRUNCATE/FLUSHDB/mock clear) runs before stage 1 only; later stages' `seed:` blocks still apply additively. Each stage arms its own mocks and runs its own `verify:` — mock recording buffers are per-stage, so `verify.calls` never sees a previous stage's traffic. `watch:` snapshots are per-stage.
- **Failure:** a FAILED/ERRORED stage stops the chain (`skip-rest` default); downstream stages report `SKIPPED (dependency 'create order happy path' failed)` — never silently green. Run exit code is 1.
- **CLI:** `testkit run 'order lifecycle'` runs the flow; `testkit run 'order lifecycle:delete order'` runs the chain **up to and including** that stage (earlier stages are prerequisites, never skipped). `testkit list` shows flows expanded with stage order. `--step` pauses across stage boundaries too.
- **Shuffle:** a flow shuffles as one atomic unit; stages never reorder internally.
- **Rejected alternative:** a free-form `needs:` DAG between test files — deferred. Explicit linear flows are deterministic, trivially reportable, and cover the create→update→delete case; a DAG scheduler can layer on later without DSL breakage.

---

## 3. Workspace architecture, traits, dependencies

### 3.1 Workspace layout

```
testkit/
├── Cargo.toml                     # [workspace]: shared lints & dep versions
└── crates/
    ├── testkit-dsl/               # schema.rs, parse.rs (discovery, YAML-path tracking),
    │                              # template.rs (scan {{ }} sites, defer render), validate.rs
    ├── testkit-store/             # LEAF: traits.rs, outcome.rs (VerifyOutcome/NearMiss/FieldDiff),
    │                              # registry.rs, error.rs — serde + async-trait only
    ├── testkit-store-postgres/    # sqlx driver: spec.rs (typed YAML doc), seed/verify/reset/watch
    ├── testkit-store-redis/       # redis driver, same shape
    ├── testkit-mock/              # server.rs, session.rs (MockHub), matcher.rs, recorder.rs, verify.rs
    ├── testkit-core/              # engine.rs, context.rs, render.rs, http.rs, capture.rs,
    │                              # assert_http.rs, jsondiff.rs (owned), result.rs (report model)
    ├── testkit-report/            # pretty.rs, json.rs, junit.rs — renderers only
    └── testkit-cli/               # main.rs, args.rs (clap), config.rs (figment), commands/
```

**Dependency graph rules:** `testkit-store` and `testkit-mock` are leaves. `core → dsl, store, mock`. Driver crates depend only on `store`. **Only the CLI knows concrete drivers** — it builds the `StoreRegistry` and hands `Arc<dyn StateStore>` to the engine. Adding MySQL = one new crate + one registration line. `testkit-dsl` knows nothing about execution, so `testkit validate` lints with zero live connections.

**The key trick — driver-owned documents:** the YAML under `seed.postgres:` / `verify.redis:` is *not* parsed into a universal schema (none fits PG tables, Redis keys, and future Mongo collections). The DSL keeps store blocks as raw values tagged by store kind; each driver deserializes and validates its own block. The engine only orchestrates *when* blocks are rendered and executed. That is what makes "MySQL tomorrow" a pure addition.

### 3.2 Trait boundary (`testkit-store`)

```rust
use async_trait::async_trait;          // AFIT isn't dyn-safe; the boundary is `dyn`
use serde_json::Value;

/// Rendered store block, converted to JSON. Schema owned by the driver.
pub type StoreDoc = Value;
pub enum DocMode { Seed, Verify, Reset, Watch }

/// One per store KIND ("postgres", "redis", later "mysql"), registered by the CLI.
#[async_trait]
pub trait StoreDriver: Send + Sync {
    fn kind(&self) -> &'static str;
    /// Static validation at suite-load time, templates as placeholders, no I/O.
    fn validate(&self, doc: &StoreDoc, mode: DocMode) -> Result<(), ValidationError>;
    async fn connect(&self, cfg: &StoreConnConfig) -> Result<Arc<dyn StateStore>, StoreError>;
}

/// One per configured INSTANCE. Flattened facade (all drivers implement all roles).
#[async_trait]
pub trait StateStore: Send + Sync {
    fn kind(&self) -> &'static str;
    fn alias(&self) -> &str;
    async fn ping(&self) -> Result<(), StoreError>;
    async fn reset(&self, spec: &ResetSpec) -> Result<(), StoreError>;
    async fn seed(&self, doc: &StoreDoc) -> Result<SeedReceipt, StoreError>;
    async fn snapshot(&self, doc: &StoreDoc) -> Result<Snapshot, StoreError>;      // watch mode
    /// NEVER Err for assertion failures — those are data in VerifyOutcome.
    /// Err is reserved for harness faults (connection lost, type error).
    async fn verify(&self, doc: &StoreDoc, opts: &VerifyOpts) -> Result<VerifyOutcome, StoreError>;
    async fn inspect(&self, query: &str) -> Result<Table, StoreError>;             // step mode, read-only
}

pub struct VerifyOpts {
    pub deadline: Option<Duration>,    // YAML `eventually:`
    pub poll_interval: Duration,       // default 100ms
    pub settle: Duration,              // default 500ms, for negative assertions
}
```

### 3.3 Outcome types — designed for great reports

```rust
pub struct VerifyOutcome { pub checks: Vec<CheckResult> }

pub enum CheckResult { Pass { description: String }, Fail(CheckFailure) }

pub struct CheckFailure {
    pub description: String,           // human phrasing of the expectation
    pub yaml_path: String,             // "verify.postgres[0].expect[1]"
    pub expected: Value,
    pub kind: FailureKind,
    pub near_misses: Vec<NearMiss>,    // ranked closest actual rows/calls/values
    pub attempts: u32,                 // eventually: poll count
    pub elapsed: Duration,
}

pub enum FailureKind {
    MissingRow,                                 // ← headline feature
    UnexpectedRow  { actual: Value },
    UnexpectedChange { before: Value, after: Value },   // watch mode
    ValueMismatch  { diffs: Vec<FieldDiff> },
    MissingKey, UnexpectedKey { key: String },
    CountMismatch  { expected: String, actual: u64 },
    MissedCall     { satisfied: u64 },
    UnexpectedCall { exchange: Value },
    OrderViolation { interleaving: Vec<(String, u64)> },
}

pub struct NearMiss { pub actual: Value, pub diffs: Vec<FieldDiff>, pub score: f32 }
pub struct FieldDiff { pub path: String, pub expected: Value, pub actual: Value }
```

The same structs render to terminal (pretty) and serialize verbatim to the JSON report — the JSON report is lossless with respect to the terminal output, never a re-parse of it.

### 3.4 Mock server internals (`testkit-mock`)

```rust
pub struct MockHub { active: arc_swap::ArcSwap<Session> }   // lock-free read on hot path

pub struct Session {
    pub key: SessionKey,                      // test id today; parallel discriminator tomorrow
    expectations: Vec<Stub>,                  // immutable once armed; per-stub `times` budget
    log: parking_lot::Mutex<Vec<RecordedExchange>>,
    unmatched_policy: UnmatchedPolicy,        // Fail(599) | Respond(status) | Passthrough(url)
}
impl MockHub {
    pub fn arm(&self, s: Session) -> SessionGuard;    // Guard disarms on Drop (panic-safe)
    pub fn drain(&self, key: &SessionKey) -> SessionReport;   // stubs + full recording log
}

pub struct RecordedExchange {
    pub seq: u64,                             // global order, for `ordered:` groups
    pub at: SystemTime,
    pub dependency: String,
    pub request: RecordedRequest,             // method, path, query, headers, raw + parsed body
    pub matched: Option<StubId>,              // None = unmatched
    pub responded: ResponseSummary,
}
```

One axum listener for the whole run, bound once at startup (the black-box target reads its config once, so the address must be stable). The handler: read active session → match in declaration order respecting `times` budgets → record unconditionally → respond. Serving is deliberately separate from verification (verify runs later against the log with strict matching + near-misses) — this two-pass design is why we build on a bare `axum` fallback handler rather than fighting wiremock's serve-time assertion model.

### 3.5 Final dependency list

| Concern | Crate | Why |
|---|---|---|
| Runtime | `tokio` (multi-thread) | Mandated; ecosystem default. |
| CLI | `clap` v4 (derive) | Standard; subcommands, completions. |
| Config layering | `figment` | flags > env > file with per-key provenance. |
| YAML | `serde` + `serde_yaml_ng` | `serde_yaml` archived; API-identical maintained fork, `deny_unknown_fields` everywhere. |
| Templating | `minijinja` | Tiny, serde-native, sandboxed; custom fns (`uuid()`, `now()`); a hand-rolled `${var}` substitutor was rejected — users immediately want filters/typing. |
| JSONPath | `serde_json_path` | RFC 9535 compliant, works on `serde_json::Value`. |
| JSON diff | own `core::jsondiff` module + `similar` for large text bodies | Structured `FieldDiff`s feed both renderers; ~200 lines we should own. |
| HTTP client | `reqwest` (rustls) | Per-request timeouts, pooling, JSON; rustls avoids OpenSSL pain in CI. |
| Mock server | `axum` 0.8 / `hyper` 1.x | One catch-all handler + our own YAML-driven matcher engine; wiremock's matchers are code-first, not data-first. |
| Postgres | `sqlx` (`postgres`, `runtime-tokio`, `chrono`, `uuid`, `json`) | Runtime query + bind API — table names come from YAML, so compile-time macros are useless here. |
| Redis | `redis` (`tokio-comp`, `ConnectionManager`) | De-facto client; auto-reconnect fits long runs; SCAN/FLUSHDB/typed gets. |
| Async traits | `async-trait` | Store boundary is `dyn`. |
| Errors | `thiserror` (libs) + `anyhow` (CLI edge) | Typed at boundaries; anyhow only where we print and exit. |
| Logging | `tracing` + `tracing-subscriber` | Spans per test/step; JSON layer for CI. |
| Terminal | `owo-colors`, `comfy-table`, `anstream`, `indicatif` | Colors respecting `NO_COLOR`/pipes, row-diff tables, progress. |
| Step-mode REPL | `rustyline` | Interactive prompt. |
| Misc | `globset`, `humantime-serde`, `arc-swap`, `parking_lot` | Filters, `5s` in YAML, lock-free session reads. |

---

## 4. Verification semantics

### 4.1 DB end-state: targeted queries + near-miss reporting (the headline feature)

**Strategy decision:** expectation-driven targeted queries are the default; snapshot-diff is opt-in `watch:` mode (§4.4). Targeted queries are cheap, precise, and produce exactly the near-miss data we want; full-table snapshots are O(table) and only needed to prove "nothing else changed".

Columns of an expected row partition into **identity columns** `I` (literal values post-templating, pushed into SQL `WHERE`) and **matcher columns** `M` (tag/operator matchers, checked client-side). Each matcher implements `matches(&self, actual) -> bool` and `describe() -> String` (used in diffs: `expected: <any uuid>`).

```
verify_table(table, expectations, count_mode):
  1. Fetch candidates ONCE per table: SELECT * WHERE (key cols) IN (distinct key tuples)
  2. Assignment: each actual row satisfies at most one expectation.
     Greedy full-match pass first; if unmatched expectations remain AND expectations
     share key tuples, redo as maximum bipartite matching (Hopcroft–Karp; sizes tiny)
     so greedy order can't cause false negatives.
  3. Near-miss for each unmatched expectation:
     candidates = unclaimed rows sharing its key tuple;
     if empty → relaxation query on the largest satisfiable subset of I, LIMIT 20.
     score(e, r) = matched columns / total columns; report top 3 (score ≥ 0.5) with a
     per-column ✓/✗ table; else "no similar row found (N rows for key-space, M total)".
  4. count: exact → every unclaimed row in the key-space = UNEXPECTED_ROW.
```

Rendered failure (this is the product):

```
✗ postgres: table `orders` — 1 of 2 expected rows MISSING (after 5.0s, 24 attempts)
  ┌─────────────┬────────────────────────────┬──────────────────────────────┐
  │ column      │ expected                   │ actual (closest row, id=482) │
  ├─────────────┼────────────────────────────┼──────────────────────────────┤
  │ user_id     │ 1                          │ 1                          ✓ │
  │ status      │ confirmed                  │ pending                    ✗ │
  │ total_cents │ 4200                       │ 4200                       ✓ │
  │ created_at  │ <within 5s of test start>  │ 2026-09-23T10:14:02Z       ✓ │
  └─────────────┴────────────────────────────┴──────────────────────────────┘
```

**Redis:** same expectation/matcher model, `TYPE`-dispatched reads (GET/HGETALL/etc., subset matchers for collections), TTL range checks, `pattern:` via cursor SCAN (never KEYS). Absence and `count: 0` get settle semantics.

### 4.2 Outbound-call verification

An **assignment problem with count intervals**: expectation `i` absorbs between `lo_i` and `hi_i` recorded calls; each call absorbed by at most one expectation.

1. Build match edges (method, path, query subset, header subset, body partial) between expectations and recordings — after optional `dedupe:` collapses target retries by a JSONPath key.
2. Satisfy lower bounds via maximum bipartite matching (greedy-by-specificity is wrong when `POST /send count:1` and `POST /send body:{...} count:1` can claim the same call).
3. Absorb extras up to `hi`; leftover recordings → `UNEXPECTED_CALL` (default `fail` **only for dependencies declared in `mocks:`** — traffic nobody expected to a mocked dep is nearly always a bug); over-ceiling → `TOO_MANY_CALLS`.
4. Near-miss per unsatisfied expectation: weighted similarity (dependency 3, path 3, method 2, body 2, query 1, headers 1); "right path, wrong body" renders as `method ✓ path ✓ body ✗` + JSON diff.
5. `ordered:` groups check absorbed `seq` numbers form a chain (strict) or a valid increasing selection (`subsequence` mode); violations print the interleaving with seqs and timestamps.

Unexpected-call detection runs only after the mock has been **quiet for `settle`** (recording log exposes `last_recorded_at`) so async fire-and-forget calls are caught rather than raced.

### 4.3 Eventual consistency: `eventually:`

Any end-state assertion accepts `eventually: 5s` or the long form `{timeout, interval: 100ms, backoff: 1.5, max_interval: 1s}`.

- **Positive assertions** pass as soon as one poll fully succeeds; at deadline, the failure report is built from the last poll, annotated `after 5.0s (23 attempts)`, and the near-miss shown is the best-scoring candidate **across all polls** (a row that briefly appeared with the wrong status is the most useful diagnostic).
- **Negative assertions** (absence, `count: 0`, `lte`, no-unexpected-calls) get **settle semantics**: the condition must hold continuously for `settle` (default 500ms) before the deadline — an instantaneous "nothing arrived" proves nothing about in-flight async work.
- **Batching:** all `eventually` assertions in a `verify:` block share one poll loop (one candidate fetch per table per tick); satisfied assertions freeze; collect-all semantics are preserved while polling. Non-`eventually` assertions evaluate exactly once, first.

### 4.4 Watch mode (unexpected-change detection, opt-in)

`watch: [orders, payments]` → before RUN_STEPS: `SELECT pk_cols, md5(t::text) FROM t` per table (full-row capture for tables ≤1k rows or without PK). After steps: recompute, diff into `{inserted, updated, deleted}` PK sets. **Every diff must be claimed** by a matching expectation (`expect` claims inserts/updates, `expect_absent` claims deletes); unclaimed diffs → `UNEXPECTED_CHANGE` with before/after values. Documented cost: two scans per watched table per test.

### 4.5 Isolation strategy (final recommendation)

| Store | Recommendation | Rejected alternatives |
|---|---|---|
| Postgres | **`TRUNCATE t1, t2, … RESTART IDENTITY CASCADE`, one statement, before each test.** Tables come from config/suite declarations or `information_schema` discovery minus an exclusion list (migration tables). Fast, resets sequences, safe while the target holds pool connections. VALIDATE warns when a verified table isn't in the reset scope. | **Transaction rollback — impossible, documented loudly:** the black-box target owns its own pool; its writes can never be inside our transaction, and our uncommitted seeds would be invisible to it. Per-test schema / template-DB recreate force target restarts (Postgres refuses to drop a DB with active connections); template DB remains fine for *run-level* provisioning. |
| Redis | **Dedicated logical DB (e.g. db 15) + `FLUSHDB ASYNC`.** Preflight refuses db 0 without `--allow-redis-db0`. | Prefix `SCAN`+`UNLINK` is the fallback for Redis Cluster (no SELECT) — slower, leaks unprefixed keys. |

**Reset-before, not clean-after:** a crashed previous run never poisons the next; on failure the evidence (DB rows, Redis keys, mock log) stays in place for post-mortem (`--keep-state-on-failure` default on locally); order-independence holds because every test *starts* clean, not because every test promises to clean up. `testkit run --shuffle [--seed N]` audits the claim. End-of-test CLEANUP only disarms mocks and releases resources, guaranteed via `Drop` guards even on panic/ctrl-C. Known limitation (documented): the target's in-process caches don't reset with the DB; a `reset.script` hook can hit a target `/internal/reset` endpoint if one exists.

---

## 5. CLI spec

```
testkit run [PATTERN]              # all tests, or glob over suite/test names: 'orders/*', orders/create_order
    --tag <t>            repeatable, AND semantics
    --env <name>         environment from testkit.yaml (default: local)
    --step               pause at lifecycle boundaries (implies serial, requires TTY — else exit 2)
    --break-at <test:step>
    --shuffle [--seed N] # order-independence audit
    --report <path>      JSON report (also settable in config)
    --junit <path>       JUnit XML for CI
    --keep-state-on-failure[=bool]   # default on locally, off in CI profiles
    --quiet | -v | -vv   # dots | steps expanded | wire logs
testkit list [PATTERN] [--tag t]   # resolved RunPlan, no execution (shares filter code with run)
testkit validate [--live]          # parse + static checks; --live dry-runs seeds in a rolled-back txn
testkit env [--env name]           # print MOCK_*/dependency URLs to export before starting the target
```

**Step mode** (engine exposes a TTY-free `Gate`; the CLI implements the prompt): pauses after SEED, after ARM_MOCKS, after each step's request+captures, and before VERIFY_END_STATE.

| Command | Effect |
|---|---|
| `c` / `n` | continue to next pause / advance one |
| `resp` | last step's full request/response |
| `vars` | variable scope chain incl. captures |
| `calls [dep\|seq]` | mock recording log (table), or full exchange detail |
| `db <sql>` | Postgres query inside `BEGIN READ ONLY … ROLLBACK` |
| `redis <cmd>` | allow-listed read-only commands |
| `verify` | run the `verify:` block now, non-destructively, single-shot (no `eventually` wait) |
| `q` / `Q` | abort test (ERRORED, cleanup, next test) / abort run |

**Exit codes:** `0` all passed · `1` ≥1 test FAILED · `2` config/usage error (bad YAML, bad flags, `--step` without TTY) · `3` environment error (preflight failed, store unreachable mid-run) · `130` interrupted (after CLEANUP ran).

**Reports:** pretty terminal tree per test (steps ✓/✗, then verify groups, near-miss tables, one line per passing test); JSON report with `schema_version`, per-test stages/timings, every finding as structured data (kind, yaml_path, expected, near_miss diffs, attempts/elapsed) plus the full `recorded_calls` log for failed tests; optional JUnit XML. A JSON Schema of the DSL is published for editor validation.

---

## 6. Implementation roadmap

| Milestone | Scope | Exit criterion |
|---|---|---|
| **M1 — Walking skeleton** | Workspace + `testkit-dsl` (schema, parse, `deny_unknown_fields`), `testkit-core` step executor with reqwest + HTTP `expect:` matchers, pretty output, exit codes, `run`/`list`/`validate` (static only). No stores, no mocks, no templating. | `testkit run` executes one YAML test with response assertions against a live target and reports pass/fail correctly in CI. |
| **M2 — Templating & capture** | minijinja env + typing rule, per-step `capture:`, multi-step chaining, step `repeat:`, JSON report v1, preflight probes. | A two-step test captures an id from step 1 and uses it in step 2's URL. |
| **M3 — Store boundary + Postgres (the differentiator)** | `testkit-store` traits/outcome types, registry; Postgres driver: seed (rows/sql/fixtures), TRUNCATE reset, `verify.postgres` with assignment + near-miss diff tables; `validate --live`. | A missing expected insertion reports exactly which row is missing with a per-column near-miss diff. |
| **M4 — Mock server + outbound verification** | axum listener, MockHub/Session/guards, order-based matching, `times`/`responses` sequences, unconditional recording, `unmatched: fail` (599), `verify.calls` assignment algorithm + near-misses + `ordered:`, `testkit env`. | The full example test in §2.10 minus Redis runs green, and a missed/unexpected call renders with component diffs. |
| **M5 — Redis + eventual consistency + watch** | Redis driver (seed/verify/FLUSHDB reset, prefix fallback), `eventually:` shared poll loop with settle semantics, `watch:` snapshot mode. | Async-writing target passes with `eventually:`; a stray write to a watched table fails with UNEXPECTED_CHANGE. |
| **M6 — Flows (cross-test data)** | `*.flow.yaml` schema + validation (export/`flow.*` reference checks), flow runner with `reset: once` isolation, `with:` var injection, per-stage mock sessions, SKIPPED propagation, flow-aware `run`/`list`/`--shuffle`/`--step`. | The create→update→delete lifecycle flow runs green reusing `order_id` across tests; killing stage 1 marks stages 2-3 SKIPPED. |
| **M7 — DX & CI polish** | `--step` mode REPL, `--break-at`, tag/glob filtering, `--shuffle`, JUnit output, suite `_suite.yaml` defaults, `$include`/fixtures, JSON Schema publication, secret redaction in reports, docs. | A QA engineer onboards from docs alone and debugs a failing async test with `--step` + `verify`. |

---

## 7. Top risks & open questions

### Risks

| # | Risk | Mitigation |
|---|---|---|
| 1 | **YAML → PG type impedance** — YAML values must bind to arbitrary column types (timestamptz, uuid, numeric, enums, jsonb); naive binding gives cryptic mid-suite errors. | Driver caches `information_schema` column types at connect, binds with explicit casts; `validate --live` dry-runs seeds in a rolled-back transaction; type errors are harness errors naming table/column/YAML path. |
| 2 | **Isolation leaks** — target in-process caches, background jobs writing after test end, pooled state. | Reset-before-test; `settle` quiesce before verify and reset; `unmatched: fail` surfaces stray background calls; `--shuffle` audit; documented recommendation (not requirement) that targets expose a reset/drain hook. |
| 3 | **Eventual-consistency flakiness vs speed** — async assertions are racy; retries make suites slow. | `eventually:` with settle semantics built into `VerifyOpts`; failure reports carry the last/best observation so flake reports still show what was there; CI flag to stretch deadlines on slow runners. |
| 4 | **Mock fidelity ceiling** — one stable listener can't parallelize and matching semantics can surprise. | Declaration-order matching documented as *the* rule (no specificity scoring); `SessionKey`-based hub designed now for future keyed routing (discriminator header, opt-in) or N listeners with one target per lane; passthrough mode for hybrid setups. |
| 5 | **Ecosystem drift** — `serde_yaml` archived; diff/JSONPath crates vary in maintenance. | YAML isolated behind `testkit-dsl`, diffing owned in `core::jsondiff`; forks pinned at the workspace root; published JSON Schema decouples editor tooling from Rust internals; `version: 1` header in suite files. |
| 6 | **Ephemeral mock port vs pre-started target** — the target reads its env once, possibly before the mock exists. | `testkit env` prints exportable URLs for local flows; fixed-port mode for CI; `validate` warns when port 0 is configured without a documented startup ordering. |

### Open questions for the user

1. **Parallelism appetite:** v1 is serial by design (shared target/DB/Redis). Is schema-per-worker Postgres + Redis-db-per-worker + one target instance per lane worth designing for v2, and can your target be launched N times with different env?
2. **Target startup ownership:** should testkit optionally *launch* the target process (command + env injection would solve the ephemeral-port ordering problem), or is "started separately" a hard constraint?
3. **Non-JSON payloads:** how much multipart/binary body support does v1 need (mock matching + response assertions), or is urlencoded + raw-regex enough initially?
4. **Future dependency kinds:** are gRPC or message-queue (Kafka) dependencies on the horizon? The keyed-dispatch schema (`mocks.<name>`, `verify.<store>`) extends without breaking changes, but recorder/matcher work differs substantially.
5. **Parameterized tests:** is a `matrix:` feature (one test body, N input sets) needed early? The key is reserved; naming/reporting design is deferred.
6. **Secrets policy:** is env-var interpolation + report redaction of configured header names sufficient, or is a secrets-manager integration required for CI?
