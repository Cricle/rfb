#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# Scan only reviewable RFB inputs. The migration contract is the one deliberate
# exception: it names the application layer while documenting why the boundary
# exists. Do not match ordinary words such as "expires".
SCAN_PATHS=(Cargo.toml Cargo.lock rfb rfb-runtime rfb-rig .github)
forbidden='(^|[^[:alnum:]_])(xpi|XPI|Xpi)([^[:alnum:]_]|$)'
violations=$(grep -RInE --exclude-dir=target --exclude-dir=.git \
  --exclude='check-boundary.sh' --exclude='boundary-migration.md' \
  "$forbidden" "${SCAN_PATHS[@]}" 2>/dev/null || true)
if [[ -n "$violations" ]]; then
  printf '%s\n' "$violations" >&2
  printf 'RFB boundary violation: application-layer xpi reference found\n' >&2
  exit 1
fi

for manifest in "$ROOT_DIR"/Cargo.toml "$ROOT_DIR"/rfb/Cargo.toml "$ROOT_DIR"/rfb-runtime/Cargo.toml "$ROOT_DIR"/rfb-rig/Cargo.toml; do
  test -f "$manifest" || { printf 'missing manifest: %s\n' "$manifest" >&2; exit 1; }
done
printf 'RFB dependency boundary passed\n'
