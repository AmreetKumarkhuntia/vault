#!/bin/sh
# Tests under tests/<crate>/ are not auto-discovered: each file needs a [[test]] entry in the
# owning crate's Cargo.toml, or it silently never runs.
set -eu
cd "$(dirname "$0")/.."
status=0
for f in tests/*/*.rs; do
    dir=$(basename "$(dirname "$f")")
    if [ "$dir" = code ]; then manifest=tests/Cargo.toml; want="code/$(basename "$f")"
    else manifest="crates/$dir/Cargo.toml"; want="../../$f"; fi
    if ! grep -q "path = \"$want\"" "$manifest"; then
        echo "unregistered test file: $f (add a [[test]] entry to $manifest)"; status=1
    fi
done
exit $status
