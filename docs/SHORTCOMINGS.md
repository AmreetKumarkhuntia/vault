# Vault shortcomings tracker

This is the canonical backlog for limitations in Vault itself. Items are ordered roughly from
small, high-confidence safety fixes to larger capabilities. Work them in this order unless a
later item is blocking a real adoption.

Repository-specific CI wiring, the breadth of an adopter's test suite, choosing HTTP-only mode,
and behavior of the application under test do not belong here. Keep completed entries so the
reason, fix, pull request, and first fixed release remain traceable.

## Tracking rules

- Status is one of **Open**, **In progress**, **Done**, or **Accepted limitation**.
- Keep at most one item **In progress**.
- A completed item records its pull request and first released version.
- Acceptance criteria are the definition of done; implementation details may evolve in the PR.
- Effort is a relative planning aid: XS, S, M, or L.

## Work order

| Order | ID | Status | Effort | Shortcoming |
| ---: | --- | --- | --- | --- |
| 1 | VLT-001 | Done | XS | Empty test selections exit successfully |
| 2 | VLT-002 | Done | XS | Requested report write failures do not fail the run |
| 3 | VLT-013 | Done | M | SQL fixture selection is limited to contained single files |
| 4 | VLT-003 | Open | S | Missing environment values are resolved leniently |
| 5 | VLT-004 | Open | S | Static validation is not environment-aware |
| 6 | VLT-005 | Open | M | Write-capable tests have no enforced safety gate |
| 7 | VLT-006 | Done | M | Mock recordings can expose secrets in failure reports |
| 8 | VLT-007 | Open | M | Failure matrices require duplicated tests |
| 9 | VLT-008 | Open | L | Target process lifecycle is external to Vault |
| 10 | VLT-009 | Open | L | Concurrent request scenarios cannot be expressed |
| 11 | VLT-010 | Open | M | Windows has no prebuilt npm binary |
| 12 | VLT-011 | Accepted limitation | L | State verification supports only Postgres and Redis |
| 13 | VLT-012 | Accepted limitation | L | Target in-process state cannot be reset by Vault |
| 14 | VLT-015 | Done | S | Settle is suite-global and every mock test waits it out |
| 15 | VLT-014 | Done | M | Mock calls cannot be attributed when tests run concurrently |
| 16 | VLT-016 | Done | XS | Outbound calls from tests without mocks are never checked |
| 17 | VLT-017 | Done | M | `watch` diffs whole tables, so every watch test locks the run |
| 18 | VLT-018 | Done | S | Tests have no per-flow identity, and default headers are not templated |
| 19 | VLT-019 | Done | M | `--parallel` has no serial tail and no isolated rerun |

## VLT-001: Empty test selections exit successfully

**Completed.** Fixed by [PR #10](https://github.com/AmreetKumarkhuntia/vault/pull/10) and first
released in v0.3.3.

**Problem.** A misspelled pattern or tag can select no tests. The runner builds an empty result,
whose folded status is `PASSED`, and exits `0`.

**Risk.** CI can be green while executing nothing.

**Evidence.** Selection has no non-empty guard in `crates/cli/src/runcmd.rs`; an empty
`RunResult` folds to success in `crates/core/src/result.rs`.

**Proposed resolution.** Resolve the plan before connecting to the target or stores. When the
selection is empty, print the supplied filters and exit with code `2`. Provide an explicit
opt-in only if a demonstrated workflow genuinely needs empty selections.

**Acceptance criteria.**

- A tag or pattern matching nothing performs no preflight or external I/O and exits `2`.
- The error names the unmatched pattern and tags.
- A non-empty selection retains existing behavior.

## VLT-002: Requested report write failures do not fail the run

**Completed.** Fixed by [PR #10](https://github.com/AmreetKumarkhuntia/vault/pull/10) and first
released in v0.3.3.

**Problem.** Failure to write a configured JSON or JUnit report only prints a warning; the test
result still determines the exit code.

**Risk.** CI can succeed without publishing an artifact that downstream jobs require.

**Evidence.** Both write errors are warning-only branches near the end of
`crates/cli/src/runcmd.rs`.

**Proposed resolution.** Treat failure to write any explicitly requested report as a harness
error and return exit code `3`, while still attempting every configured report so all errors are
shown together.

**Acceptance criteria.**

- An unwritable JSON or JUnit path makes the command exit `3`.
- All requested report writes are attempted and every failure is printed.
- Successful report generation keeps the underlying test exit code.

## VLT-013: SQL fixture selection is limited to contained single files

**Completed.** Shipped in v0.5.0 (commit `2fb5170`, YAML-controlled fixture plans): suite-global
seed, declaration-relative and filesystem-wide exact paths, deterministic SQL globs, an inspectable
frozen fixture plan, reset-aware flow behavior, and external/live/demo coverage.

**Problem.** PostgreSQL seed supports one `sql_file` at a time, resolves it from the suite root,
and rejects paths outside that root. Suites cannot declare a reusable global baseline, select an
ordered directory of SQL files, or keep shared fixtures beside another project component.

**Risk.** Authors must duplicate selector lists or copy shared data under every suite. Ad hoc file
enumeration makes execution order, reset frequency, duplicate handling, and the exact SQL chosen by
a run difficult to review. A changing glob could otherwise make validation and execution disagree.

**Evidence.** The PostgreSQL driver recognizes `table`, `sql`, and `sql_file`, and canonicalizes
every file against one suite root. `Config` has no global seed document, test discovery does not
retain a declaration directory for driver resolution, and the CLI has no fixture-plan view.

**Proposed resolution.** Add top-level `seed` to `vault.yaml` and `sql_glob` beside literal
`sql_file`. Resolve relative selectors from their declaring YAML; allow unlimited parent traversal,
absolute paths, and symlink targets anywhere. Expand globs deterministically and validate/freeze the
entire effective plan before external I/O. Re-resolve and re-read only those frozen files at
execution, then run global and local entries together under the driver-owned transaction.

**Migration impact.** Test-local relative paths change from suite-root-relative to
declaration-relative. Nested tests may need additional `../` segments; diagnostics should point to
the old suite-root location when it exists. Filesystem-wide access also makes suite YAML trusted
configuration and must be called out in release and security documentation.

**Acceptance criteria.**

- Root selectors resolve from `vault.yaml`; test selectors resolve from their individual YAML.
  Parent-relative, Unix absolute, Windows-drive, UNC, symlinked, and environment-derived paths work
  independently of process CWD.
- `sql_file` remains literal. `sql_glob` requires wildcard syntax, uses segment-aware `*`/`?` and
  recursive `**`, expands in normalized logical-path order, and rejects invalid or empty matches.
  `**` is a complete path segment and parent components precede the first wildcard.
- Every selected path is a readable regular lowercase-`.sql` UTF-8 file. Duplicate canonical files,
  including global/local and overlapping-glob duplicates, fail with both selector origins.
- The complete plan is frozen before external connections. Runtime re-resolution catches deletion,
  target changes, read errors, invalid contents, and transaction-control statements without adding
  newly matching files.
- After a reset, suite-global entries precede test-local entries. `flow reset: each` repeats both;
  `reset: once` applies the global baseline before the first executable stage and later stages apply
  only their local seed.
- Global files, local files, inline SQL, and structured rows execute in declaration order inside one
  PostgreSQL transaction; any failure rolls back the effective seed invocation.
- `vault list --fixtures` reports scope, origin, selector, logical path, resolved path, and order
  without connecting externally.
- External validation tests, a live PostgreSQL matrix, and a runnable global/exact/glob/`../../`
  demo scenario run in CI.

**Non-goals.** This item does not add generic YAML `fixture:` entries, `$include` expansion,
templating inside SQL files, `psql` meta-command support, multiple PostgreSQL stores per environment,
or additional SQL engines.

## VLT-003: Missing environment values are resolved leniently

**Problem.** Config-time `${env.NAME}` without a default becomes an empty string when `NAME` is
unset. Test-time templates use MiniJinja's default lenient undefined behavior, so missing values
can also render empty instead of failing at their point of use.

**Risk.** Missing secrets and misspelled variables can become empty request headers, URLs, or
payload values and produce misleading application failures.

**Evidence.** `crates/dsl/src/yaml.rs` uses an empty fallback for missing variables, and
`crates/core/src/template.rs` creates a default MiniJinja environment without strict undefined
handling.

**Proposed resolution.** Make missing values errors by default. Preserve explicit defaults,
including an explicitly empty default (`${env.NAME:-}`), and report the variable name and YAML
origin without printing secret values.

**Acceptance criteria.**

- Missing `${env.NAME}` fails parsing with the file and variable name.
- `${env.NAME:-fallback}` and `${env.NAME:-}` remain supported.
- Missing `{{ env.NAME }}` fails the affected test as a template error without exposing values.
- Tests cover config files, test files, headers, variables, and nested defaults.

## VLT-004: Static validation is not environment-aware

**Problem.** `vault validate` checks the DSL and store document shapes, but it cannot select an
environment and does not verify that the environment configures every store referenced by the
suite. That check happens only in `vault run`.

**Risk.** A suite can pass static validation and then fail immediately when run in CI.

**Evidence.** The `Validate` command has no `--env` option in `crates/cli/src/main.rs`, while
`validate_environment_stores` is called only by the run path in `crates/cli/src/runcmd.rs`.

**Proposed resolution.** Add `vault validate --env <name>`, defaulting to `local`, and reuse the
same selected-environment checks as `run` without opening network connections.

**Acceptance criteria.**

- An unknown environment exits `2` with a clear error.
- Missing Postgres, Redis, or future store configuration is found statically for the selected
  environment.
- Validation remains offline and does not contact targets or stores.

## VLT-005: Write-capable tests have no enforced safety gate

**Problem.** Tags such as `writes` are conventions. Vault has no protected-environment marker or
explicit confirmation before executing write-capable tests.

**Risk.** A correct test command can mutate the wrong reachable environment.

**Evidence.** The current CLI filters arbitrary tags but attaches no safety semantics to them in
`crates/cli/src/main.rs` and `crates/cli/src/runcmd.rs`.

**Proposed resolution.** Introduce explicit test effect metadata and require an affirmative CLI
flag for write-capable selections unless the selected environment is declared disposable. Do not
infer safety solely from HTTP methods or tag spelling.

**Acceptance criteria.**

- Write-capable tests cannot start against a non-disposable environment without explicit opt-in.
- Read-only suites require no new flag.
- `list` shows the resolved effect classification.
- Configuration and CLI errors occur before preflight or test requests.

## VLT-006: Mock recordings can expose secrets in failure reports

**Status.** Implemented. CLI report copies are masked after matching and flow exports complete,
before terminal summaries, JSON, JUnit, and HTML are rendered. Standard credential headers and
common secret fields are masked by default; `report.redact` adds headers, fields, JSONPath
selectors, and text patterns. Private request, response-header, capture, and mock credential
sources prevent echoed secrets from leaking even when HTML lifecycle tracing is disabled.
Dedicated integration tests cover report formats, raw matching, partial failures, and protected
report identities. Live interactive inspection remains separate from generated reports.

**Problem.** Failed tests retain complete mocked-dependency request headers and bodies. Those
recordings are serialized into JSON reports without a redaction pass.

**Risk.** Application credentials or personal data sent to a mocked dependency can enter CI logs
and artifacts.

**Evidence.** `crates/mock/src/record.rs` records full headers and raw/JSON bodies, and
`crates/core/src/result.rs` includes those recordings for non-passing tests.

**Proposed resolution.** Redact standard credential headers by default and support configured
header names and JSON paths. Perform redaction before data reaches terminal or report structs so
all renderers receive the safe form.

**Acceptance criteria.**

- Authorization, cookies, and common API-key headers are redacted by default.
- Custom header names and JSON paths can be configured.
- Matchers continue to evaluate the original unredacted request in memory.
- Terminal, JSON, and JUnit output never receive the original configured secret fields.

## VLT-007: Failure matrices require duplicated tests

**Problem.** The DSL has no parameterized or matrix test construct.

**Risk.** Validation and failure-path coverage becomes repetitive, hard to review, and likely to
drift between copied files.

**Evidence.** `TestDef` in `crates/dsl/src/schema.rs` represents one variable set and has no case
matrix; discovery expands files only, not cases.

**Proposed resolution.** Add a `cases` collection that runs one test body once per named variable
set and gives every case an independent result and report name.

**Acceptance criteria.**

- Every case has a required stable name and its own variables.
- Filtering, reports, flows, and failure output identify the case unambiguously.
- One failing case does not hide results from the remaining cases.
- Existing tests without cases remain unchanged.

## VLT-008: Target process lifecycle is external to Vault

**Problem.** Vault assumes the target is already running. Adopters must independently allocate a
port, inject mock URLs, start the process, wait for readiness, preserve logs, and stop it.

**Risk.** Every repository builds a different wrapper script, often with weaker cleanup and
diagnostics.

**Evidence.** The CLI exposes run, list, validate, and env commands only; preflight in
`crates/cli/src/preflight.rs` probes an already-started target.

**Proposed resolution.** Add an optional managed target command with environment injection,
readiness checking, log capture, signal forwarding, and guaranteed cleanup. Preserve the current
external-target mode.

**Acceptance criteria.**

- Vault can start a configured command after allocating required mock endpoints.
- Readiness failure includes captured target output.
- Normal completion, test failure, timeout, and interruption all stop the child process.
- Existing externally managed targets continue to work without configuration changes.

## VLT-009: Concurrent request scenarios cannot be expressed

**Problem.** Steps within a test execute serially; the DSL has no parallel request group.
`run --parallel` overlaps whole flows and tests, not requests inside one test.

**Risk.** Race conditions, duplicate concurrent submissions, and locking guarantees cannot be
verified with Vault itself.

**Evidence.** `crates/core/src/engine.rs` awaits each step in sequence, and the v1 limitation is
documented in `README.md`.

**Proposed resolution.** Add an explicit parallel step group with a start barrier, deterministic
result ordering, independent captures, and a bounded concurrency limit. Keep suites serial by
default because shared stores still require isolation.

**Acceptance criteria.**

- A test can release multiple HTTP requests from one barrier.
- Every branch has independent assertions, timing, and captures.
- Output ordering is deterministic even when completion order is not.
- Timeouts and cancellation cleanly join all branches.

## VLT-010: Windows has no prebuilt npm binary

**Problem.** The npm wrapper supports Linux and macOS x64/arm64 only; Windows users must build
from source.

**Risk.** `npx` is not a complete cross-platform installation path.

**Evidence.** Supported targets in `npm/bin/vault.js` omit Windows, and `npm/README.md` directs
Windows users to Cargo.

**Proposed resolution.** Add a Windows release target, archive format handling, checksum coverage,
and wrapper extraction/launch support.

**Acceptance criteria.**

- Release CI publishes and verifies an x64 Windows asset.
- The npm wrapper downloads, checksums, caches, and runs it.
- A registry-backed Windows smoke test exercises `npx vault --run`.

## VLT-011: State verification supports only Postgres and Redis

**Problem.** MySQL, MongoDB, and other state stores have no bundled drivers.

**Risk.** Adopters using other stores can only use HTTP and mock verification.

**Evidence.** The concrete registry in `crates/cli/src/runcmd.rs` registers only the Postgres and
Redis drivers.

**Current decision.** Accepted limitation until a real adopter supplies requirements. The driver
boundary is intentionally extensible; add one driver at a time rather than designing a universal
store schema.

**Acceptance criteria for reopening.** Record the requesting adopter, required seed/reset/verify
semantics, isolation strategy, and an end-to-end fixture before implementation begins.

## VLT-012: Target in-process state cannot be reset by Vault

**Problem.** Resetting Postgres or Redis cannot clear caches, workers, or other state held inside
the target process.

**Risk.** Later tests may observe state that survived store reset, especially when the target has
background work or caches.

**Evidence.** This is documented under `README.md` notes and limitations; Vault currently owns no
target lifecycle or reset protocol.

**Current decision.** Accepted limitation until VLT-008 defines target lifecycle. Adopters should
restart the target or expose a test-only reset/drain endpoint from their wrapper.

**Acceptance criteria for reopening.** Define an authenticated reset contract, ordering relative
to store reset, timeout behavior, and failure reporting without making application-specific hooks
mandatory.

## VLT-015: Settle is suite-global and every mock test waits it out

**Completed.** Implemented on branch `feat/parallel-lanes` as one commit on top of v0.8.1; record the pull request and first released version on merge.

**Problem.** `defaults.verify.settle` is one value for the whole suite. After its steps, every test
that declares `mocks` or `verify.calls` sleeps until no call has arrived for `settle`
(`crates/core/src/engine.rs:726-739`):

- the wait is capped at `max(4×settle, 2s)`;
- it is checked every 50 ms;
- every `eventually` verify that passes waits `settle` again to confirm.

So a suite that needs a long window for one slow async path makes every mock test pay that window.

**Risk.** Most of the run time is idle waiting. Lowering the global value saves time, but it also
shrinks the window for absence checks (`count: 0`, `unexpected: fail`) in the few tests that need
it.

**Evidence.** Vayu suite, Vault 0.8.1 (`de0be35`), 2026-10-22, `settle: 500ms`:

- mock quiet-wait: about 169.5 s over 394 tests;
- settle re-checks: 69 × 500 ms = 34.5 s;
- together that is 204 s of a 400.9 s loop, against 84.9 s of real HTTP time;
- the latest outbound call observed landed 53 ms after its test's last step.

**Example.** A typical mock test: its steps finish at 120 ms, the last mock call arrives at
140 ms, and the test then idles until 640 ms before the next test starts.

**Proposed resolution.**

- Allow `verify.settle` per test, overriding `defaults.verify.settle`. A flow stage is a test, so
  no separate per-flow value.
- Sleep exactly the remaining quiet time instead of 50 ms ticks.

**Acceptance criteria.**

- A test `verify.settle:` overrides the default.
- The quiet-wait ends within a few ms of `last call + settle` (or the cap), not on the next 50 ms
  tick.
- The report shows the settle value each test used.
- Existing suites behave the same when they set no override.

## VLT-014: Mock calls cannot be attributed when tests run concurrently

**Completed.** Implemented on branch `feat/parallel-lanes` as one commit on top of v0.8.1; record the pull request and first released version on merge.

**Problem.** The mock listener holds one active session (`crates/mock/src/server.rs:15,50-57`).
`arm()` replaces it, and the guard of the test that armed last clears it
(`crates/mock/src/session.rs:166-175`). So `run --parallel` gives every test that declares
`mocks`, `verify.calls` or `watch` the run-wide write lock (`crates/core/src/engine.rs:175-180`),
and those tests never overlap. On a shared target, the target rarely tells Vault which test caused
a call:

- it forwards no correlation header;
- many dependency base URLs are fixed when it starts.

**Risk.** Suites that verify outbound calls, the case Vault is built for, cannot run concurrently.
Removing the lock without a routing rule misattributes calls in both directions:

- false failures;
- `count: 0` guards that pass because the stray call went to another test's session.

**Evidence.** Vayu suite, Vault 0.8.1 (`de0be35`), 2026-10-22.

- **The lock dominates.** 460 of 738 executions take it, 319 of 400.9 s. A simulated `--parallel`
  with today's lock stays around 320 s for any N from 2 to 8.
- **The target gives no handle.** Of 1048 recorded outbound calls, 0 carry `x-request-id`.
  435 (41.5%) go to base URLs fixed at target start, such as Euler at
  `TEST_ENV_BASE_URL + "/euler"`.
- **Stub matching cannot route.** About 60% of calls also match a stub from another flow.
- **Prefix ownership already holds.** In the same green run every call had exactly one owning
  mount prefix: 0 `<unknown>` calls in tests with mocks, 0 calls into stub-less mounts.

**Example (today, without the lock).**

| When | What happens | Active session |
| --- | --- | --- |
| t0 | Lane 1 arms session A (an `euler` stub) | A |
| t1 | Lane 2 arms session B (a `platform` stub); `arm()` replaces A | B |
| t2 | The target calls `/euler/v1/offers/list` for lane 1. It resolves against B, is recorded as `<unknown>` and answered 599. Lane 1's step fails; lane 2 fails `verify.unexpected` for a call it never caused | B |
| t3 | Lane 2 ends; its guard clears the slot | none |
| t4 | Lane 1's next call gets `599 vault-mock: no active session` | none |

**Proposed resolution: prefix-ownership routing.** Mock `prefix` is rendered through templates,
but `mock.<name>.base_url` is built from the raw mount (`engine.rs:256-265`), so prefixes are
static in practice and a test can claim them before its clock starts.

- **Claims.** A test claims every mount prefix of its `mocks`, all or nothing, before it arms.
  Claims are exclusive: two claims conflict when one prefix is a segment-prefix of the other (the
  root mount conflicts with everything). A conflicting claim waits; the wait is not test time.
- **Routing.** The listener keeps every armed session and routes each request to the session
  whose mount is the longest prefix of the path. No holder: recorded as `<unknown>` in every
  armed session, answered 599.
- **Serial runs.** `--parallel 1` still has one armed session at a time, so nothing changes.
- **Lock.** Only `watch` tests keep the run-wide write lock.

| Test | Mock declared | Claim |
| --- | --- | --- |
| A | `euler` with stubs | `/euler` |
| B | `euler: {}` with `count: 0` | `/euler` |
| C | `platform`, prefix `/platform/arch-store`, with stubs | `/platform/arch-store` |
| D | `platform`, prefix `/platform/s`, with stubs | `/platform/s` |

A and B wait for each other. C and D run alongside either of them.

**Later relaxation (not in the first cut).** Stub-less mounts with `unmatched: fail` and only
`count: 0` expectations (B above) could share a prefix as Guards, with orphan calls broadcast to
every Guard; the simulation puts that at about 92 s against 124 s at 8 lanes. Also deferred:
fairness for waiting claims, a run-level orphan log, and a per-call route column in the report.

**Known limitation.** Suppose a test starts calling a prefix it did not declare, while another
test owns that prefix with an absorbing expectation (unbounded count or `unexpected: allow`). That
test passes in parallel, although the same call is `<unknown>` in a serial run. Keep a serial run
as the backstop.

**Non-goals.**

- Routing by a header the target forwards; that needs a change in the target.
- One isolated store set per lane; not every adopter can provide it.
- Concurrent steps inside one test (VLT-009).

**Acceptance criteria.**

- Tests cover the conflict rule (segment boundaries, root mount), routing with one holder and for
  orphans, that a conflicting claim waits for the holder's guard to drop, and that dropping a
  session removes only that session.
- An integration run with two concurrent tests on different prefixes passes with every call in its
  own session.
- `--parallel 1` gives identical results before and after the change.

## VLT-016: Outbound calls from tests without mocks are never checked

**Completed.** Implemented on branch `feat/parallel-lanes` as one commit on top of v0.8.1; record the pull request and first released version on merge.

**Problem.** Every test arms a mock session (`crates/core/src/engine.rs:353-371`), but its
recordings are drained and verified only when the test declares `mocks` or `verify.calls`
(`engine.rs:472`). So when a test with no mocks makes the target call a dependency, each call is
answered 599 and nothing reports it.

**Risk.** A real outbound call, or a regression that adds one, goes unnoticed in serial runs.

**Evidence.** Vayu suite, 2026-10-22: 39 calls from 13 executions of 9 test definitions were
answered 599 and never reported. They went to `/euler` and `/platform/arch-store`; one example is
`cases/shipping/default-provider/login.test.yaml`. All of those tests passed.

**Example.** A login case declares no `mocks:`. Its request makes the target call `/euler/...`,
which gets a 599, and the case still passes because none of its assertions depends on that call.

**Proposed resolution.** Always drain the session. A test with no mocks that recorded any call
fails with "undeclared outbound call", listing each call's method, path and dependency. These tests
get no quiet-wait, so they stay as fast as today. `defaults.mock.unmatched: allow` keeps today's
behaviour for suites that want it.

**Ordering.** Before VLT-014, tests without mocks overlap under the read lock and `arm()` replaces
the single active slot, so a stray call lands in whichever lane armed last. This check is only
reliable in parallel runs once prefix routing exists.

**Acceptance criteria.**

- The failure lists every recorded call.
- Tests that record nothing are unaffected and take no extra time.
- With VLT-014 in place the check behaves the same in serial and parallel runs.

## VLT-017: `watch` diffs whole tables, so every watch test locks the run

**Completed.** Implemented on branch `feat/parallel-lanes` as one commit on top of v0.8.1; record the pull request and first released version on merge.

**Problem.** `watch` snapshots and diffs whole tables (`crates/store-postgres/src/lib.rs:517-581`),
so a test with `watch` takes the run-wide write lock (`crates/core/src/engine.rs:176`). On a shared
baseline, rows that any other lane writes appear in the diff.

**Risk.** Even with mock routing (VLT-014), every watch test still serializes the run.

**Evidence.** Vayu suite: 302 tests, 885 watched-table entries, about 165 s of the run. Simulated
at 8 lanes with mock routing and settle 200 ms:

- whole-table watch: about 187 s;
- scoped watch: about 124 s.

**Example.**

```yaml
watch:
  - cart                          # today: every row of cart, so the run locks
  - table: cart                   # proposed: only rows this flow owns
    where:
      shopId: "{{ vars.shop_id }}"
```

**Proposed resolution.**

- Accept `{table, where}` entries. Render `where` and apply it in the snapshot query as a jsonb
  containment filter (`to_jsonb(t) @> $1`), so the diff only reads matching rows.
- A test whose watch entries are all scoped takes no run-wide lock.
- An unscoped entry keeps today's behaviour.

**Acceptance criteria.**

- A scoped diff ignores rows outside the filter, and the filter runs in SQL.
- Unscoped entries behave as today and still take the lock; scoped-only tests do not.
- The docs say that a scoped watch no longer proves "nothing else in the table changed".

## VLT-018: Tests have no per-flow identity, and default headers are not templated

**Completed.** Implemented on branch `feat/parallel-lanes` as one commit on top of v0.8.1; record the pull request and first released version on merge.

**Problem.** The template context holds `vars`, `steps`, `mock`, `env`, `flow` (exports only) and
`test.name` (`crates/core/src/engine.rs:267-275`). It has no flow name or id. Default request
headers are sent as raw strings (`crates/core/src/http.rs:85-88`). Suites that share a baseline
across flows need:

- flow-unique keys in seeds, requests and assertions;
- a per-flow value in default headers.

**Risk.** Authors either hand-write unique ids in every file, or keep fixed ids that collide when
two flows run the same stage. A shared session header couples unrelated flows.

**Evidence.** Vayu suite:

- Every request sends `X-Session-Id: vault-ci`. Vayu keys `session_tasks:<session>` on it, so one
  flow's task creation cancels other flows' abandonment tasks.
- 19 shared stages cannot be re-run, because they use a fixed `authKey.entity` or a fixed shop id.
- One phone number is used by 62 flows.

**Example.**

```yaml
defaults:
  request:
    headers:
      X-Session-Id: "vault-{{ flow.uid }}"   # today this would be sent literally
seed:
  postgres:
    - sql: "INSERT INTO shop (id, url) VALUES ('shop-{{ flow.uid }}', '…')"
```

**Proposed resolution.**

- Add `flow.uid` (a stable short hash of the flow name) and `flow.name` to the template context
  of every stage, and `test.uid` to every test.
- Render default headers per test, as step headers already are.
- Inline `sql:` and row seeds already render templates; `sql_file` contents do not (VLT-013
  non-goal), so file-based seeds cannot use the id.
- Undefined template variables rendering empty is VLT-003.

**Acceptance criteria.**

- The values exist in stage templates and in inline seed SQL.
- Every stage of a flow gets the same `flow.uid`, and it stays the same across runs.
- Default headers are rendered.

## VLT-019: `--parallel` has no serial tail and no isolated rerun

**Completed.** Implemented on branch `feat/parallel-lanes` as one commit on top of v0.8.1; record the pull request and first released version on merge.

**Problem.** With `--parallel N`, every selected item runs in lanes on one shared baseline
(`crates/cli/src/runcmd.rs:845`). A suite has no way to:

- keep a few flows serial with their usual reset, such as flows that change global state;
- re-check a lane failure in isolation.

**Risk.** One global-state flow forces the whole suite back to serial. A failure caused by a
co-running test looks the same as a real one.

**Evidence.** Vayu suite: about 10 flows change global state. They cover subscription sweeps,
`GLOBAL_CONFIG`, process-wide A/B options and a global abandonment queue.

**Example.** 208 flows run across 8 lanes. The 10 flows tagged for serial execution then run one
at a time with reset.

**Proposed resolution.**

- `--serial-tag <tag>` (repeatable): items carrying such a tag run after the lanes drain, serially,
  with per-item reset. Everything else runs in lanes on the shared baseline.
- Follow-up, not in the first cut: after the lanes drain, rerun each failed parallel item serially
  from a full reset and keep both attempts in the report.

**Acceptance criteria.**

- Tagged items always get their reset and start only after every lane has finished.
- Results stay in plan order in every report.
- `--parallel 1` is unchanged.

## Explicitly out of scope

The following findings belong in the adopting repository or application, not this tracker:

- Jenkins, GitHub Actions, or other repository-specific CI wiring.
- Missing domain scenarios in an adopter's suite.
- Choosing not to configure Vault's optional Postgres or Redis verification.
- Application-specific uniqueness, idempotency, or business-policy decisions.
- The unconfirmed claim that Vault 0.3.2 does not interpolate `${env...}` in test files. The
  common parser currently applies config-time interpolation to every YAML file; preserve a minimal
  reproduction before opening a separate defect.
