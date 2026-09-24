#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
  echo "usage: $0 <rust-target> <tag> [output-dir]" >&2
  exit 2
fi

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TARGET=$1
TAG=$2
OUTPUT=${3:-"$ROOT/dist"}
NAME="vault-${TAG}-${TARGET}"
BINARY="$ROOT/target/${TARGET}/release/vault"

if [[ ! -x "$BINARY" ]]; then
  echo "release package: binary is not executable: $BINARY" >&2
  exit 1
fi

mkdir -p "$OUTPUT/$NAME"
cp "$BINARY" "$ROOT/README.md" "$ROOT/LICENSE" "$OUTPUT/$NAME/"
tar -C "$OUTPUT" -czf "$OUTPUT/$NAME.tar.gz" "$NAME"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$OUTPUT" && sha256sum "$NAME.tar.gz" >"$NAME.tar.gz.sha256")
else
  (cd "$OUTPUT" && shasum -a 256 "$NAME.tar.gz" >"$NAME.tar.gz.sha256")
fi

echo "$OUTPUT/$NAME.tar.gz"
