#!/usr/bin/env bash
# End-to-end demo: build everything, start the demo target wired at the mock
# server, run the suite, exercise CLI safety cases, then run the deliberately
# failing showcase tests.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

PG_URL="${VAULT_PG_URL:-postgres://$USER@localhost:5432/vault_demo}"
REDIS_URL="${VAULT_REDIS_URL:-redis://127.0.0.1:6379/15}"
DEMO_TMP="$(mktemp -d "${TMPDIR:-/tmp}/vault-demo.XXXXXX")"
TARGET_PID=""

cleanup() {
  if [[ -n "$TARGET_PID" ]]; then
    kill "$TARGET_PID" 2>/dev/null || true
  fi
  if [[ -n "$DEMO_TMP" && -d "$DEMO_TMP" ]]; then
    rm -rf -- "$DEMO_TMP"
  fi
}
trap cleanup EXIT

echo "── building ──────────────────────────────────────────"
cargo build --workspace

echo
echo "── empty-selection safety (run=2, list=0) ──────"
EMPTY_JSON="$DEMO_TMP/empty-selection.json"
EMPTY_JUNIT="$DEMO_TMP/empty-selection.xml"
if ./target/debug/vault run 'demo-does-not-exist-*' \
  --suite-dir tests/flows \
  --tag smoke \
  --report "$EMPTY_JSON" \
  --junit "$EMPTY_JUNIT" \
  >"$DEMO_TMP/empty-run.log" 2>&1; then
  EMPTY_RUN_CODE=0
else
  EMPTY_RUN_CODE=$?
fi
cat "$DEMO_TMP/empty-run.log"
if [[ "$EMPTY_RUN_CODE" -ne 2 ]]; then
  echo "demo check failed: empty run returned $EMPTY_RUN_CODE, expected 2" >&2
  exit 1
fi
if ! grep -Fq 'pattern: demo-does-not-exist-*' "$DEMO_TMP/empty-run.log" || \
  ! grep -Fq 'tags: smoke' "$DEMO_TMP/empty-run.log"; then
  echo "demo check failed: empty-run diagnostic omitted the requested filters" >&2
  exit 1
fi
if [[ -e "$EMPTY_JSON" || -e "$EMPTY_JUNIT" ]]; then
  echo "demo check failed: an empty run wrote a report" >&2
  exit 1
fi

if ./target/debug/vault list 'demo-does-not-exist-*' \
  --suite-dir tests/flows \
  --tag smoke \
  >"$DEMO_TMP/empty-list.log" 2>&1; then
  EMPTY_LIST_CODE=0
else
  EMPTY_LIST_CODE=$?
fi
cat "$DEMO_TMP/empty-list.log"
if [[ "$EMPTY_LIST_CODE" -ne 0 ]] || \
  ! grep -Fq '0 flows, 0 standalone tests' "$DEMO_TMP/empty-list.log"; then
  echo "demo check failed: empty list did not report zero matches with exit 0" >&2
  exit 1
fi

if command -v createdb >/dev/null; then
  createdb vault_demo 2>/dev/null || true
fi

echo
echo "── starting demo-target (logs: demo-target.log) ─────"
PORT=8091 \
DATABASE_URL="$PG_URL" \
REDIS_URL="$REDIS_URL" \
PAYMENTS_URL=http://127.0.0.1:9095/payments \
EMAIL_URL=http://127.0.0.1:9095/email \
./target/debug/demo-target > demo-target.log 2>&1 &
TARGET_PID=$!

echo "── running the suite ────────────────────────────────"
./target/debug/vault run --suite-dir tests/flows -v

echo
echo "── report-write safety (one fails, sibling remains) ──"
mkdir -p "$DEMO_TMP/workdir"
printf '%s\n' 'blocks JSON report parent creation' >"$DEMO_TMP/workdir/report-parent"
GOOD_JUNIT="$DEMO_TMP/workdir/vault-junit.xml"
BAD_JSON="$DEMO_TMP/workdir/report-parent/vault-report.json"
if (
  cd "$DEMO_TMP/workdir"
  "$REPO_ROOT/target/debug/vault" run 'get order not found' \
    --suite-dir "$REPO_ROOT/tests/flows" \
    --report report-parent/vault-report.json \
    --junit vault-junit.xml
) >"$DEMO_TMP/report-write.log" 2>&1; then
  REPORT_RUN_CODE=0
else
  REPORT_RUN_CODE=$?
fi
cat "$DEMO_TMP/report-write.log"
if [[ "$REPORT_RUN_CODE" -ne 3 ]]; then
  echo "demo check failed: report-write failure returned $REPORT_RUN_CODE, expected 3" >&2
  exit 1
fi
if [[ ! -s "$GOOD_JUNIT" ]]; then
  echo "demo check failed: successful JUnit sibling report was not retained" >&2
  exit 1
fi
if [[ -e "$BAD_JSON" ]]; then
  echo "demo check failed: invalid JSON report path unexpectedly succeeded" >&2
  exit 1
fi
echo "verified: JSON failed, JUnit remains, and the run exited 3"

echo
echo "── failure showcase (exit 1 is the point) ───────────"
if VAULT_RUN_NEGATIVE= ./target/debug/vault run --suite-dir tests/flows -t negative; then
  NEGATIVE_CODE=0
else
  NEGATIVE_CODE=$?
fi
if [[ "$NEGATIVE_CODE" -ne 1 ]]; then
  echo "demo check failed: negative showcase returned $NEGATIVE_CODE, expected 1" >&2
  exit 1
fi
