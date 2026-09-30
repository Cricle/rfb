#!/usr/bin/env bash
# 版本单一来源：sdk/VERSION 是唯一手写版本号的地方，本脚本把它传播到全部
# 发布清单。bump 流程 = `scripts/sync-versions.sh 0.0.3`（改 sdk/VERSION 并
# 同步全部清单），之后 release_contract.rs 与 release.yml 的版本门都以
# sdk/VERSION 为锚点校验一致性。
#
#   scripts/sync-versions.sh           # 把 sdk/VERSION 传播到全部清单
#   scripts/sync-versions.sh 0.0.3     # 先写 sdk/VERSION，再传播
#   scripts/sync-versions.sh --check   # 干跑：逐条校验每个 pattern 与当前
#                                      # sdk/VERSION 一致，不写任何文件；
#                                      # 任一失配则列出全部失配文件并
#                                      # exit 1（release.yml 版本门使用）。
#
# 覆盖的清单（与 rfb/tests/release_contract.rs 的断言一一对应）：
#   sdk/python/pyproject.toml [project] version
#   sdk/nodejs/package.json 顶层 "version" + package-lock.json 两处（root 与
#     packages[""]——漏掉会让每次 npm install 都产生 lock 重写 diff）
#   sdk/java/pom.xml project <version>
#   sdk/java/tests/pom.xml project <version> + rfb-sdk 依赖 <version>（前两处）
#   sdk/examples/java/pom.xml <version> 两处（project version + rfb-sdk 依赖）
#   sdk/csharp/src/Rfb.Sdk/Rfb.Sdk.csproj <Version>
#   sdk/csharp/Rfb.Cli/Rfb.Cli.csproj <Version>
#   Cargo.toml [workspace.package] version + [workspace.dependencies] 两处
#   sdk/examples/rust/Cargo.toml rfb-sdk 依赖 version
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT_DIR"

mode="write"
if [ "${1:-}" = "--check" ]; then
  mode="check"
  shift
fi

if [ $# -ge 1 ]; then
  if [ "$mode" = "check" ]; then
    printf 'sync-versions.sh --check 不接受版本参数：它只比对当前 sdk/VERSION\n' >&2
    exit 2
  fi
  printf '%s\n' "$1" > sdk/VERSION
fi
version=$(tr -d '[:space:]' < sdk/VERSION)
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf 'sdk/VERSION: not a semver: %s\n' "$version" >&2
  exit 1
fi

# 精确重写（无 BOM、保持各文件原格式）：每条 pattern 必须命中 count 次，
# 否则报错退出——清单结构变化时宁失败不漏改。--check 干跑同一条 pattern
# 列表：逐条比对"重写结果 == 当前内容"（不一致即该处清单与 sdk/VERSION
# 漂移），收集全部失配后一次性列出并 exit 1，全程零写入。
python3 - "$version" "$mode" <<'PY'
import re
import sys
from pathlib import Path

version = sys.argv[1]
mode = sys.argv[2]
root = Path.cwd()
problems = []


def sub(path, pattern, repl, count=1):
    p = root / path
    text = p.read_text(encoding="utf-8")
    if text.startswith("\ufeff"):
        text = text[1:]
    new, n = re.subn(pattern, repl, text, count=count, flags=re.M)
    if n != count:
        if mode == "write":
            raise SystemExit(f"{path}: expected {count} match(es), found {n}")
        problems.append(f"{path}: expected {count} match(es), found {n}")
        return
    if mode == "check":
        if new == text:
            print(f"  {path} == {version}")
        else:
            problems.append(f"{path}: version != {version} (stale)")
        return
    # UTF-8 explicitly without BOM: strict readers (json.load, tomllib)
    # reject a BOM.
    p.write_text(new, encoding="utf-8", newline="")
    print(f"  {path} -> {version}")


sub("sdk/python/pyproject.toml", r'(?m)^version = "[^"]*"',
    f'version = "{version}"')
sub("sdk/nodejs/package.json", r'(?m)^(\s*)"version": "[^"]*"',
    rf'\g<1>"version": "{version}"')
# package-lock.json：root 顶层 + packages[""] 两处（结构固定，恰好是文件里
# 最早的 "version" 行）。
sub("sdk/nodejs/package-lock.json", r'"version": "[^"]*"',
    f'"version": "{version}"', 2)
# pom.xml / tests/pom.xml：字面量 <version> 按出现顺序计数——主 pom 的第一处
# 是 project version；tests/pom 前两处是 project version + rfb-sdk 依赖
# （后续的 ${junit.version} 等属性引用不匹配字面量模式之外的……它们也匹配
# <version>.*</version>，因此按 count 截断即可，顺序由文件结构保证）。
sub("sdk/java/pom.xml", r"<version>[^<]*</version>",
    f"<version>{version}</version>", 1)
sub("sdk/java/tests/pom.xml", r"<version>[^<]*</version>",
    f"<version>{version}</version>", 2)
# 示例 pom（quickstart 对发布包的引用版本）。
sub("sdk/examples/java/pom.xml", r"<version>[^<]*</version>",
    f"<version>{version}</version>", 2)
for csproj in ("sdk/csharp/src/Rfb.Sdk/Rfb.Sdk.csproj",
               "sdk/csharp/Rfb.Cli/Rfb.Cli.csproj"):
    sub(csproj, r"<Version>[^<]*</Version>",
        f"<Version>{version}</Version>")
# workspace manifest：crate 版本单一来源（rfb/tests/release_contract.rs 锚点）。
sub("Cargo.toml", r'(\[workspace\.package\]\nversion = ")[^"]*(")',
    rf'\g<1>{version}\g<2>')
# [workspace.dependencies] 的 crate 间版本要求（path + version 双钉）。
sub("Cargo.toml", r'(rfb-runtime = \{ path = "rfb-runtime", version = ")[^"]*(")',
    rf'\g<1>{version}\g<2>')
sub("Cargo.toml", r'(rfb = \{ package = "rfb-sdk", path = "rfb", version = ")[^"]*(")',
    rf'\g<1>{version}\g<2>')
sub("sdk/examples/rust/Cargo.toml",
    r'rfb-sdk", version = "[^"]*"',
    f'rfb-sdk", version = "{version}"')

if problems:
    print("version check failed (run scripts/sync-versions.sh to sync):",
          file=sys.stderr)
    for item in problems:
        print(f"  {item}", file=sys.stderr)
    sys.exit(1)
if mode == "check":
    print(f"all release manifests already at {version}")
else:
    print(f"synced release manifests to {version}")
PY
