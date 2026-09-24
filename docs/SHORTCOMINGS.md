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
| 1 | VLT-001 | Open | XS | Empty test selections exit successfully |
| 2 | VLT-002 | Open | XS | Requested report write failures do not fail the run |
| 3 | VLT-003 | Open | S | Missing environment values are resolved leniently |
| 4 | VLT-004 | Open | S | Static validation is not environment-aware |
| 5 | VLT-005 | Open | M | Write-capable tests have no enforced safety gate |
| 6 | VLT-006 | Open | M | Mock recordings can expose secrets in failure reports |
| 7 | VLT-007 | Open | M | Failure matrices require duplicated tests |
| 8 | VLT-008 | Open | L | Target process lifecycle is external to Vault |
| 9 | VLT-009 | Open | L | Concurrent request scenarios cannot be expressed |
| 10 | VLT-010 | Open | M | Windows has no prebuilt npm binary |
| 11 | VLT-011 | Accepted limitation | L | State verification supports only Postgres and Redis |
| 12 | VLT-012 | Accepted limitation | L | Target in-process state cannot be reset by Vault |

## VLT-001: Empty test selections exit successfully

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

**Problem.** Tests, flows, and steps execute serially; the DSL has no parallel request group.

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

## Explicitly out of scope

The following findings belong in the adopting repository or application, not this tracker:

- Jenkins, GitHub Actions, or other repository-specific CI wiring.
- Missing domain scenarios in an adopter's suite.
- Choosing not to configure Vault's optional Postgres or Redis verification.
- Application-specific uniqueness, idempotency, or business-policy decisions.
- The unconfirmed claim that Vault 0.3.2 does not interpolate `${env...}` in test files. The
  common parser currently applies config-time interpolation to every YAML file; preserve a minimal
  reproduction before opening a separate defect.

