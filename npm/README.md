# @amreetkumarkhuntia/vault

npx wrapper for [vault](https://github.com/AmreetKumarkhuntia/vault) — a black-box HTTP API test harness with YAML-defined HTTP checks, optional Postgres/Redis verification, and a recording dependency mock.

On first run it downloads the matching prebuilt binary from GitHub Releases (sha256-verified) and caches it; after that it's instant.

```sh
# Installed shorthand:
npm install --save-dev @amreetkumarkhuntia/vault
npx vault --run ./tests/flows

# One-off usage:
npx @amreetkumarkhuntia/vault --help
npx @amreetkumarkhuntia/vault validate --suite-dir tests/flows
npx @amreetkumarkhuntia/vault --run tests/flows/vault.yaml -t smoke
```

`--run` accepts either a suite directory or its `vault.yaml` file. Tests and flows are discovered recursively below that directory.

Postgres and Redis are optional. An HTTP-only suite simply omits those stores from `vault.yaml` and does not use their seed or verify blocks.

**Supported platforms:** Linux x64/arm64 (static musl binaries, including Alpine) and macOS x64/arm64.

On Windows, build from source instead:

```sh
cargo install --git https://github.com/AmreetKumarkhuntia/vault vault
```

Repository contributors can test the unpublished wrapper against a local build with `make npm-smoke`; the wrapper's explicit `VAULT_BINARY` override bypasses release downloading for that smoke test.

Docs, the YAML test language, and examples: https://github.com/AmreetKumarkhuntia/vault
