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
make demo        # full guided demo: build, start demo-target, suite, failure showcase
```

Day-to-day commands (`make help` lists all):

```sh
make test        # Rust tests for the harness code (tests/code)
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

To exercise the packed npm CLI from this checkout before a release exists:

```sh
make npm-smoke
```

Exit codes: `0` pass · `1` a test failed · `2` config/usage error · `3` environment/preflight error.

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
  report/          pretty terminal / JSON / JUnit renderers (same structs, lossless)
  cli/             the `vault` binary
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

- Execution is serial by design: one target, one database. The session abstractions reserve hooks for parallel lanes.
- Transaction-rollback isolation is impossible for a black-box target (it owns its own connections) — that's why isolation is TRUNCATE-based and documented as such.
- When enabled, Redis verification requires a dedicated logical DB (e.g. `/15`); the driver refuses db 0 without `allow_db0: true`.
- The target's in-process caches don't reset with the DB; if your server has a reset endpoint, call it via a seed `sql:`-style hook or an extra step.
- `skip: "${env.FLAG:-reason}"` gates a test on an environment variable — an empty value means "run it" (see `tests/negative/`).
