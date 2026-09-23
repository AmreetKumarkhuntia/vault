#!/usr/bin/env bash
# End-to-end demo: build everything, start the demo target wired at the mock
# server, run the suite, then run the deliberately-failing showcase tests.
set -euo pipefail
cd "$(dirname "$0")/.."

PG_URL="${VAULT_PG_URL:-postgres://$USER@localhost:5432/vault_demo}"
REDIS_URL="${VAULT_REDIS_URL:-redis://127.0.0.1:6379/15}"

if command -v createdb >/dev/null; then
  createdb vault_demo 2>/dev/null || true
fi

echo "── building ──────────────────────────────────────────"
cargo build --workspace

echo "── starting demo-target (logs: demo-target.log) ─────"
PORT=8091 \
DATABASE_URL="$PG_URL" \
REDIS_URL="$REDIS_URL" \
PAYMENTS_URL=http://127.0.0.1:9095/payments \
EMAIL_URL=http://127.0.0.1:9095/email \
./target/debug/demo-target > demo-target.log 2>&1 &
TARGET_PID=$!
trap 'kill $TARGET_PID 2>/dev/null || true' EXIT

echo "── running the suite ─────────────────────────────────"
./target/debug/vault run --suite-dir tests -v

echo
echo "── failure showcase (exit 1 is the point) ───────────"
VAULT_RUN_NEGATIVE= ./target/debug/vault run --suite-dir tests -t negative || true
