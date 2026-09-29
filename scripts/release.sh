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

# 开头先跑边界门：token 扫描 + 依赖方向断言（rfb-sdk 不依赖 rfb-rig/rfb-ben、
# rfb-runtime 不依赖 rfb-sdk/rfb-rig、rfb-rig 依赖 rfb-sdk）。发布出去的
# 依赖图一旦倒置，crates.io 上无法撤回，必须在打包前拦下。
bash scripts/check-boundary.sh

version=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; p=json.load(sys.stdin)["packages"]; print(next(x["version"] for x in p if x["name"] == "rfb-sdk"))')
# Every published crate must share the tag's version: publish_crate() skips
# versions already on crates.io, so a partial bump would silently publish a
# new tag with stale rfb-runtime/rfb-rig dependencies.
for crate in rfb-runtime rfb-rig; do
  crate_version=$(CRATE_NAME="$crate" cargo metadata --no-deps --format-version 1 | python3 -c 'import json,os,sys; p=json.load(sys.stdin)["packages"]; print(next(x["version"] for x in p if x["name"] == os.environ["CRATE_NAME"]))')
  if [[ "$crate_version" != "$version" ]]; then
    printf '%s version %s does not match rfb-sdk version %s\n' "$crate" "$crate_version" "$version" >&2
    exit 1
  fi
done
tag=$(git describe --tags --exact-match 2>/dev/null || true)
if [[ "$mode" == publish && -z "$tag" ]]; then
  printf 'formal release requires an exact version tag\n' >&2
  exit 1
fi
if [[ -n "$tag" && "$tag" != "v$version" ]]; then
  printf 'tag %s does not match workspace version %s\n' "$tag" "$version" >&2
  exit 1
fi

# fmt 不涉及依赖解析（无 --locked 可言）；clippy/test/doc 全部 --locked：
# 发布机与 Cargo.lock 必须逐字节一致，防止本地/远端依赖漂移污染发布产物。
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked --no-fail-fast
RUSTDOCFLAGS='-D missing_docs -D warnings' cargo doc --workspace --all-features --locked --lib --no-deps

# Dry-run verification only: package each crate and simulate publishing. The
# rfb-sdk/rfb-rig dry-runs fail while rfb-runtime@0.0.1 is unpublished (known
# limitation, documented in the release runbook).
if [[ "$mode" == dry-run ]]; then
  # The main crate ships as `rfb-sdk` (crates.io bare name `rfb` is occupied by
  # an unrelated 2022 crate; the lib target keeps the name `rfb`). Publish order
  # follows the dependency chain: rfb-runtime <- rfb-sdk <- rfb-rig.
  for crate in rfb-runtime rfb-sdk rfb-rig; do
    cargo package -p "$crate" --locked
    cargo publish -p "$crate" --locked --dry-run
  done
  printf 'Release checks passed for workspace version %s\n' "$version"
  exit 0
fi

: "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN is required}"

# Package and publish strictly in dependency order. rfb-sdk/rfb-rig cannot be
# packaged until rfb-runtime 0.0.1 is resolvable on crates.io, so packaging
# everything up front breaks first-time publishing of a new crate family.
# Idempotent: a crate/version already on crates.io is skipped, so re-running
# a release (after fixing an unrelated channel) never conflicts.
CRATES_UA="rfb-release-script (contact: qnydhuaji@gmail.com)"
crate_published() {
  curl -s -o /dev/null -w "%{http_code}" -H "User-Agent: $CRATES_UA" \
    "https://crates.io/api/v1/crates/$1/$version" | grep -q 200
}
publish_crate() {
  if crate_published "$1"; then
    printf '%s %s already on crates.io; skipping\n' "$1" "$version"
    return 0
  fi
  cargo package -p "$1" --locked
  # --token on the command line: the CARGO_REGISTRIES_CRATES_IO_TOKEN env var
  # was not honored by the runner's cargo during one release run ("no token
  # found"); a CLI flag cannot be lost to environment plumbing.
  cargo publish -p "$1" --locked --token "$CARGO_REGISTRY_TOKEN"
}
publish_crate rfb-runtime
printf 'rfb-runtime published; waiting %ss for index propagation\n' "${CRATES_IO_PROPAGATION_SECONDS:-30}"
sleep "${CRATES_IO_PROPAGATION_SECONDS:-30}"
publish_crate rfb-sdk
printf 'rfb-sdk published; waiting %ss for index propagation\n' "${CRATES_IO_PROPAGATION_SECONDS:-30}"
sleep "${CRATES_IO_PROPAGATION_SECONDS:-30}"
publish_crate rfb-rig
printf 'Published workspace version %s\n' "$version"
