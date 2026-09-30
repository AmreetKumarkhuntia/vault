# @thunderkiller/vault

npx wrapper for [vault](https://github.com/AmreetKumarkhuntia/vault) — a black-box HTTP API test harness with YAML-defined HTTP checks, optional Postgres/Redis verification, and a recording dependency mock.

On first run it downloads the matching prebuilt binary from GitHub Releases (sha256-verified) and caches it; after that it's instant.

```sh
# Installed shorthand:
npm install --save-dev @thunderkiller/vault
npx vault --run ./tests/flows

# One-off usage:
npx @thunderkiller/vault --help
npx @thunderkiller/vault validate --suite-dir tests/flows
npx @thunderkiller/vault --run tests/flows/vault.yaml -t smoke
```

`--run` accepts either a suite directory or its `vault.yaml` file. Tests and flows are discovered recursively below that directory.

PostgreSQL `sql_file` and `sql_glob` selectors are passed through unchanged. Vault resolves relative
selectors from the YAML file that declares them, so `../`/`../../`, absolute paths, and fixtures
outside the suite work the same through `npx` as they do through the native binary. `sql_file` is
literal; `sql_glob` uses `/` separators with segment-local `*`/`?` and a whole-segment recursive
`**`. Because suite YAML can select and execute any accessible `.sql` file, run only trusted suites
when filesystem access or database credentials are available.

Use the global `--no-color` flag before or after a subcommand for plain CI logs, including with the
shorthand:

```sh
npx vault --no-color --run tests/flows
```

The binary automatically strips ANSI escapes for redirected stdout/stderr. A non-empty `NO_COLOR`
or `CLICOLOR=0` also disables color and takes priority over `CLICOLOR_FORCE`; an empty `NO_COLOR`
is treated as unset. Unicode status glyphs remain unchanged, and JSON/JUnit files are never styled.

An empty `run` selection exits `2` before connecting to the target, stores, or mock server; `list` remains a successful zero-result inspection. If a requested JSON or JUnit report cannot be written, Vault still attempts every other requested report and exits `3`.

Postgres and Redis are optional. An HTTP-only suite simply omits those stores from `vault.yaml` and does not use their seed or verify blocks.

**Supported platforms:** Linux x64/arm64 (static musl binaries, including Alpine) and macOS x64/arm64.

On Windows, build from source instead:

```sh
cargo install --git https://github.com/AmreetKumarkhuntia/vault vault
```

Repository contributors can test the unpublished wrapper against a local build with `make npm-smoke`; the wrapper's explicit `VAULT_BINARY` override bypasses release downloading for that smoke test.

Docs, the YAML test language, and examples: https://github.com/AmreetKumarkhuntia/vault
