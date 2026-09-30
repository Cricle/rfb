#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# Scan every reviewable input in the repository. The forbidden
# application-layer token is assembled at runtime so this script itself never
# contains a literal occurrence of it — the repository must stay clean.
# Do not match ordinary words such as "expires".
token="$(printf 'x%s' "$(printf 'p%s' i)")"
violations=$(grep -RIniE --exclude-dir=target --exclude-dir=.git --exclude-dir=dist \
  "(^|[^[:alnum:]_])${token}([^[:alnum:]_]|$)" "$ROOT_DIR" 2>/dev/null || true)
if [[ -n "$violations" ]]; then
  printf '%s\n' "$violations" >&2
  printf 'RFB boundary violation: forbidden application-layer reference found\n' >&2
  exit 1
fi

for manifest in "$ROOT_DIR"/Cargo.toml "$ROOT_DIR"/rfb/Cargo.toml "$ROOT_DIR"/rfb-runtime/Cargo.toml "$ROOT_DIR"/rfb-rig/Cargo.toml; do
  test -f "$manifest" || { printf 'missing manifest: %s\n' "$manifest" >&2; exit 1; }
done

# ---------------------------------------------------------------------------
# 依赖方向断言（防依赖倒置）：
#   - crate `rfb-sdk` 的依赖集合不得包含 `rfb-rig` / `rfb-ben`；
#   - crate `rfb-runtime` 的依赖集合不得包含 `rfb-sdk` / `rfb-rig`；
#   - crate `rfb-rig` 必须依赖 `rfb-sdk`。
# 用 `cargo metadata --no-deps` 解析而不是 grep Cargo.toml：metadata 能看穿
# `rfb = { package = "rfb-sdk", ... }` 这类重命名（rfb-rig / ben 都这样写），
# 按真实包名断言更稳。工具缺失时的降级策略分两档：本机开发环境优雅降级为
# 警告（不让本机工具缺失挡住边界扫描）；CI（CI=true）必须硬失败——CI 容器
# 里静默跳过等于断言永远不跑（rust:1-bookworm 容器曾因无 python3 而把本
# 断言静默降级成通过）。
# ---------------------------------------------------------------------------
if [ "${CI:-}" = "true" ]; then
  degrade() {
    # $1: 缺失/不可用的工具描述
    printf 'error: CI=true but %s unavailable; refusing to skip the dependency-direction check\n' "$1" >&2
    exit 1
  }
else
  degrade() {
    printf 'warning: %s unavailable; dependency-direction check skipped\n' "$1" >&2
  }
fi

if ! command -v cargo >/dev/null 2>&1 || ! command -v python3 >/dev/null 2>&1; then
  degrade "cargo or python3"
  # CI=true 时 degrade 已 exit 1；本机走到这里=警告后视为通过（跳过断言）。
  exit 0
fi

metadata_file=$(mktemp)
if ! cargo metadata --no-deps --format-version 1 > "$metadata_file" 2>/dev/null; then
  rm -f "$metadata_file"
  degrade "cargo metadata"
  exit 0
fi

dep_rc=0
# 注意：python 代码体必须顶格（python 对缩进敏感），heredoc 结束符也是。
dep_out=$(python3 - "$metadata_file" 2>&1 <<'PY'
import json, sys

meta = json.load(open(sys.argv[1], encoding="utf-8"))
deps = {p["name"]: set() for p in meta["packages"]}
for p in meta["packages"]:
    for d in p["dependencies"]:
        # 含 optional / target 专属 / dev 依赖：任何方向的倒置都算违反。
        deps[p["name"]].add(d["name"])

bad = []
# rfb-sdk 是纯契约/门面层：不得反向依赖集成层（rfb-rig）与基准层（rfb-ben）。
for banned in ("rfb-rig", "rfb-ben"):
    if banned in deps.get("rfb-sdk", ()):
        bad.append("rfb-sdk -> " + banned + " (forbidden: sdk must not depend on rig/ben)")
# rfb-runtime 是最底层运行时：不得依赖 SDK 或任何集成层。
for banned in ("rfb-sdk", "rfb-rig"):
    if banned in deps.get("rfb-runtime", ()):
        bad.append("rfb-runtime -> " + banned + " (forbidden: runtime must not depend on sdk/rig)")
# rfb-rig 是 SDK 之上的集成层：必须显式依赖 rfb-sdk。
if "rfb-sdk" not in deps.get("rfb-rig", ()):
    bad.append("rfb-rig missing dependency rfb-sdk (rig must depend on sdk)")

for b in bad:
    print("RFB dependency direction violation: " + b, file=sys.stderr)
sys.exit(1 if bad else 0)
PY
) || dep_rc=$?
rm -f "$metadata_file"
if [ "$dep_rc" -ne 0 ]; then
  if [ -n "$dep_out" ]; then
    # 断言脚本自报的违反项（python 已打印明细）：任何环境都硬失败。
    printf '%s\n' "$dep_out" >&2
    exit 1
  fi
  # python3 存在但不可执行（如 Windows Store 占位 stub：rc=49、无输出）：
  # 按工具缺失处理——CI 硬失败；本机降级为警告并注明 rc。
  degrade "python3 (present but rc=$dep_rc, no output)"
fi

printf 'RFB dependency boundary passed\n'
