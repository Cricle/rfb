#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# Scan every reviewable input in the repository. The forbidden
# application-layer token is assembled at runtime so this script itself never
# contains a literal occurrence of it — the repository must stay clean.
# Do not match ordinary words such as "expires".
token="$(printf 'x%s' "$(printf 'p%s' i)")"
violations=$(grep -RIniE --exclude-dir=target --exclude-dir=.git --exclude-dir=dist \
  "(^|[^[:alnum:]_])${token}([^[:alnum:]_]|$)" . 2>/dev/null || true)
if [[ -n "$violations" ]]; then
  printf '%s\n' "$violations" >&2
  printf 'RFB boundary violation: forbidden application-layer reference found\n' >&2
  exit 1
fi

for manifest in "$ROOT_DIR"/Cargo.toml "$ROOT_DIR"/rfb/Cargo.toml "$ROOT_DIR"/rfb-runtime/Cargo.toml "$ROOT_DIR"/rfb-rig/Cargo.toml; do
  test -f "$manifest" || { printf 'missing manifest: %s\n' "$manifest" >&2; exit 1; }
done
printf 'RFB dependency boundary passed\n'
