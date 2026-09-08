#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT_DIR"

mode="publish"
case "${1:-}" in
  "") ;;
  --check|--dry-run) mode="dry-run" ;;
  *) printf 'usage: %s [--check|--dry-run]\n' "$0" >&2; exit 2 ;;
esac

version=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; p=json.load(sys.stdin)["packages"]; print(next(x["version"] for x in p if x["name"] == "rfb"))')
tag=$(git describe --tags --exact-match 2>/dev/null || true)
if [[ "$mode" == publish && -z "$tag" ]]; then
  printf 'formal release requires an exact version tag\n' >&2
  exit 1
fi
if [[ -n "$tag" && "$tag" != "v$version" ]]; then
  printf 'tag %s does not match workspace version %s\n' "$tag" "$version" >&2
  exit 1
fi

cargo fmt --all -- --check
cargo test --workspace --all-features --no-fail-fast
RUSTDOCFLAGS='-D missing_docs -D warnings' cargo doc --workspace --all-features --lib --no-deps
for crate in rfb-runtime rfb rfb-rig; do
  cargo package -p "$crate" --locked --allow-dirty
  if [[ "$mode" == dry-run ]]; then
    cargo publish -p "$crate" --locked --dry-run
  fi
done

if [[ "$mode" == dry-run ]]; then
  printf 'Release checks passed for workspace version %s\n' "$version"
  exit 0
fi

: "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN is required}"
export CARGO_REGISTRIES_CRATES_IO_TOKEN="$CARGO_REGISTRY_TOKEN"
unset CARGO_REGISTRY_TOKEN

publish_crate() { cargo publish -p "$1" --locked; }
publish_crate rfb-runtime
sleep "${CRATES_IO_PROPAGATION_SECONDS:-30}"
publish_crate rfb
sleep "${CRATES_IO_PROPAGATION_SECONDS:-30}"
publish_crate rfb-rig
printf 'Published workspace version %s\n' "$version"
