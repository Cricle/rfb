#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
failed=0
while IFS= read -r -d '' file; do
  printf 'embedded test module found: %s\n' "$file" >&2
  failed=1
done < <(
  find "$ROOT_DIR/rfb" "$ROOT_DIR/rfb-rig" "$ROOT_DIR/rfb-runtime" \
    -path '*/src/*' -name '*.rs' -print0 |
    xargs -0 grep -lZE '#[[:space:]]*\[cfg[[:space:]]*\([[:space:]]*test([,)]|[[:space:]])' 2>/dev/null || true
)

for crate in rfb rfb-rig rfb-runtime; do
  test -d "$ROOT_DIR/$crate/tests" || { printf 'missing tests directory: %s\n' "$crate" >&2; failed=1; }
done

if (( failed )); then
  exit 1
fi
printf 'tests-folder check passed\n'
