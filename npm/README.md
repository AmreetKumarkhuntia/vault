# @amreetkumarkhuntia/vault

npx wrapper for [vault](https://github.com/AmreetKumarkhuntia/vault) — a black-box HTTP API test harness with YAML-defined tests, seeded Postgres/Redis, and a recording dependency mock.

On first run it downloads the matching prebuilt binary from GitHub Releases (sha256-verified) and caches it; after that it's instant.

```sh
npx @amreetkumarkhuntia/vault --help
npx @amreetkumarkhuntia/vault validate --suite-dir tests/flows
npx @amreetkumarkhuntia/vault run --suite-dir tests/flows -t smoke
```

**Supported platforms:** Linux x64 and arm64 (static musl binaries — works on Alpine too).

On macOS/Windows, build from source instead:

```sh
cargo install --git https://github.com/AmreetKumarkhuntia/vault vault
```

Docs, the YAML test language, and examples: https://github.com/AmreetKumarkhuntia/vault
