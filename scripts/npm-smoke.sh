#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
SMOKE_TMP=$(mktemp -d "${TMPDIR:-/tmp}/vault-npm-smoke.XXXXXX")
SERVER_PID=""

cleanup() {
  if [[ -n "$SERVER_PID" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf -- "$SMOKE_TMP"
}
trap cleanup EXIT

if [[ -n "${VAULT_BINARY:-}" ]]; then
  VAULT_BIN=$VAULT_BINARY
else
  cargo build --manifest-path "$ROOT/Cargo.toml" -p vault
  VAULT_BIN="$ROOT/target/debug/vault"
fi

if [[ ! -x "$VAULT_BIN" ]]; then
  echo "npm smoke: vault binary is not executable: $VAULT_BIN" >&2
  exit 1
fi

PORT_FILE="$SMOKE_TMP/http-port"
VAULT_HTTP_PORT_FILE="$PORT_FILE" node "$ROOT/tests/http-only/server.js" \
  >"$SMOKE_TMP/http-server.log" 2>&1 &
SERVER_PID=$!

for _ in {1..100}; do
  [[ -s "$PORT_FILE" ]] && break
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    cat "$SMOKE_TMP/http-server.log" >&2
    exit 1
  fi
  sleep 0.05
done
if [[ ! -s "$PORT_FILE" ]]; then
  echo "npm smoke: HTTP fixture did not become ready" >&2
  cat "$SMOKE_TMP/http-server.log" >&2
  exit 1
fi

TARBALL=$(cd "$ROOT/npm" && npm_config_cache="$SMOKE_TMP/npm-cache" \
  npm pack --pack-destination "$SMOKE_TMP" --silent)
mkdir -p "$SMOKE_TMP/project"
cd "$SMOKE_TMP/project"
npm_config_cache="$SMOKE_TMP/npm-cache" npm init --yes --silent >/dev/null
npm_config_cache="$SMOKE_TMP/npm-cache" npm install --ignore-scripts --no-audit --no-fund \
  "$SMOKE_TMP/$TARBALL" >/dev/null

VAULT_HTTP_URL="http://127.0.0.1:$(<"$PORT_FILE")" \
VAULT_BINARY="$VAULT_BIN" \
npx --no-install vault --run "$ROOT/tests/http-only"
