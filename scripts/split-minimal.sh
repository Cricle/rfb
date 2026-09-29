#!/usr/bin/env bash
# 剥离手术：把当前 rfb 树变换为 forkd-only 简版树（确定性、可重复）。
#
#   scripts/split-minimal.sh [TARGET_DIR]      # 生成简版树（默认 ./minimal-dist）
#   scripts/split-minimal.sh --check [TARGET]  # 生成 + 编译门禁验证
#
# 原则：
# - 源树（main）永远不动；简版每次从最新 main 重新生成（"同步迭代 = 重新
#   手术"——没有长期分支、没有 merge、没有冲突）。
# - 依赖结构约定（主分支纪律）：zeroboot 的命令面/实现/测试全部集中在
#   zeroboot 专属文件里；共享路由文件（commands/dispatch/mod/lib/Cargo.toml）
#   的挂接点各只有几行——手术 = 删文件 + 删这几行。
# - 自校验：简版树里 zeroboot 引用只剩白名单类（共享 wire 的
#   zeroboot_protocol/ZBRT 帧、rootfs 镜像格式名、fail-closed 分支、注释），
#   出现其他引用即失败。
# - --check 跑简版编译门禁（forkd-only feature 组合），失败退出 1——发布
#   流水线用它做门。
set -euo pipefail

SRC_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CHECK=0
if [ "${1:-}" = "--check" ]; then CHECK=1; shift; fi
TARGET=${1:-$SRC_ROOT/minimal-dist}

echo "== 1. 复制源树（排除构建产物）=="
rm -rf "$TARGET"
mkdir -p "$TARGET"
(cd "$SRC_ROOT" && tar cf - \
  --exclude=.git --exclude=target --exclude=.cargo --exclude=node_modules \
  --exclude=minimal-dist --exclude=.github \
  .) | tar xf - -C "$TARGET"
cd "$TARGET"

echo "== 2. 删除 zeroboot 专属文件 =="
REMOVE=(
  rfb/src/zeroboot/provider.rs
  rfb/src/zeroboot/verification.rs
  rfb/src/cli/zeroboot.rs
  rfb/src/cli/zeroboot_backend.rs
  rfb/src/vsock.rs
  rfb/tests/zeroboot_concurrency.rs
  rfb/tests/zeroboot_firecracker.rs
  rfb/tests/zeroboot_fork.rs
  rfb/tests/zeroboot_interpreters.rs
  rfb/tests/zeroboot_provider.rs
  rfb/tests/zeroboot_provider_real.rs
  rfb/tests/zeroboot_real_e2e.rs
  rfb/tests/zeroboot_session.rs
  rfb/tests/zeroboot_v1_contract.rs
  rfb/tests/zeroboot_vsock.rs
  rfb/tests/vsock_boundaries.rs
  rfb/tests/provider.rs
  rfb/tests/zeroboot_rootfs_staging.rs
  rfb-runtime/src/zeroboot_connection.rs
  rfb-runtime/src/zeroboot_guest.rs
  rfb-runtime/tests/zeroboot_connection.rs
  rfb-runtime/tests/zeroboot_guest.rs
  ben
)
for path in "${REMOVE[@]}"; do
  rm -rf "$path"
  echo "  removed $path"
done

echo "== 3. 挂接点行级手术 =="
python3 - <<'PY'
import re
from pathlib import Path


def drop_lines(path, matchers):
    """按谓词删行；matcher(line)->True 的行（连同其紧邻的前导 #[cfg]/#[path]/
    /// 行）被删除。返回删除数。"""
    p = Path(path)
    lines = p.read_text(encoding="utf-8").splitlines(keepends=True)
    out, removed = [], 0
    for line in lines:
        if any(m(line) for m in matchers):
            while out and (out[-1].strip().startswith("#[cfg(")
                           or out[-1].strip().startswith("#[path")
                           or out[-1].strip().startswith("///")):
                out.pop()
            removed += 1
            continue
        out.append(line)
    p.write_text("".join(out), encoding="utf-8", newline="")
    return removed


def drop_block(path, start_marker):
    """删除 start_marker 所在行起、大括号平衡的块（含前导属性行）。"""
    p = Path(path)
    lines = p.read_text(encoding="utf-8").splitlines(keepends=True)
    out, i, removed = [], 0, 0
    while i < len(lines):
        line = lines[i]
        if start_marker in line:
            while out and (out[-1].strip().startswith("#[")
                           or out[-1].strip().startswith("///")):
                out.pop()
            depth = line.count("{") - line.count("}")
            i += 1
            while i < len(lines) and depth > 0:
                depth += lines[i].count("{") - lines[i].count("}")
                i += 1
            removed += 1
            continue
        out.append(line)
        i += 1
    if removed == 0:
        raise SystemExit(f"{path}: block {start_marker!r} not found")
    p.write_text("".join(out), encoding="utf-8", newline="")


# rfb/src/cli/mod.rs：zeroboot 模块注册行。
drop_lines("rfb/src/cli/mod.rs",
           [lambda l: l.strip() in ("pub mod zeroboot;", "pub mod zeroboot_backend;")])

# rfb/src/lib.rs：zeroboot 家族声明的手术。
# - protocol：共享 wire（RFB1/SDK 都引用）→ 去 zeroboot feature 门，保留。
# - firecracker：**通用 Firecracker HTTP 驱动，rfb1 的 boot 复用它** →
#   门从 zeroboot feature 改为 unix（模块文件保留）。
# - vsock：rfb1 也用 connect_vsock_uds → 保留（文件不删；本脚本第 2 步的
#   REMOVE 若含它则此处无操作）。
# - zeroboot（provider 汇聚模块）：删除。
p = Path("rfb/src/lib.rs")
t = p.read_text(encoding="utf-8")
t = t.replace(
    '#[cfg(feature = "zeroboot")]\n'
    "/// ZBRT binary frame types and codec used by the ZeroBoot provider.\n"
    "pub mod protocol;",
    "/// ZBRT binary frame types and codec used by the ZeroBoot provider.\n"
    "pub mod protocol;")
t = t.replace(
    '#[cfg(all(feature = "zeroboot", target_os = "linux"))]\n'
    '#[path = "zeroboot/firecracker.rs"]\n'
    "pub mod firecracker;",
    '#[cfg(all(unix, feature = "forkd"))]\n'
    '#[path = "zeroboot/firecracker.rs"]\n'
    "pub mod firecracker;")
p.write_text(t, encoding="utf-8", newline="")
drop_lines("rfb/src/lib.rs",
           [lambda l: re.match(r'pub mod (vsock|zeroboot);', l.strip()) is not None])
# zeroboot/mod.rs 删了 provider/verification 后只剩 firecracker 挂接——改写。
p = Path("rfb/src/zeroboot/mod.rs")
if p.exists():
    p.write_text(
        "//! Firecracker HTTP-API driver shared by the RFB1 boot path.\n"
        "\n"
        "pub use firecracker::*;\n",
        encoding="utf-8", newline="")


# rfb/src/cli/skills.rs：帮助文案去 zeroboot verify。
p = Path("rfb/src/cli/skills.rs")
t = p.read_text(encoding="utf-8").replace(
    "rfb-cli image build-all --help; rfb-cli zeroboot verify --help",
    "rfb-cli image build-all --help")
p.write_text(t, encoding="utf-8", newline="")

# commands.rs：Zeroboot 变体（cli 面）。zeroboot.rs 的删除已带走类型定义。
p = Path("rfb/src/cli/commands.rs")
t = p.read_text(encoding="utf-8")
t = t.replace(
    "    /// ZeroBoot ZBRT real-VM acceptance and benchmark.\n"
    "    #[cfg(unix)]\n"
    "    Zeroboot {\n"
    "        /// ZeroBoot acceptance/benchmark subcommand.\n"
    "        #[command(subcommand)]\n"
    "        command: crate::cli::zeroboot::ZerobootCommand,\n"
    "    },\n", "")
p.write_text(t, encoding="utf-8", newline="")

# dispatch.rs：Zeroboot 分支行。
drop_lines("rfb/src/cli/dispatch.rs",
           [lambda l: "CommandLine::Zeroboot" in l])

# rfb/Cargo.toml：zeroboot feature 删除 + cli 收口 + tempfile/libc 归属 +
# rfb-runtime 依赖去 optional（protocol 是共享 wire，no-default 也要编译）。
p = Path("rfb/Cargo.toml")
t = p.read_text(encoding="utf-8")
t = re.sub(r'\nzeroboot = \[[^\]]*\]', "", t, count=1)
t = t.replace('"forkd", "zeroboot"]', '"forkd", "dep:tempfile", "dep:libc"]')
t = t.replace(
    'rfb-runtime = { workspace = true, optional = true }',
    'rfb-runtime = { workspace = true }')
t = t.replace('cli = ["dep:clap", "dep:sha2", "dep:toml", "dep:rfb-runtime", ',
              'cli = ["dep:clap", "dep:sha2", "dep:toml", ')
p.write_text(t, encoding="utf-8", newline="")

# 根 Cargo.toml：简版 workspace 去掉 ben（压测依赖 zeroboot）。
p = Path("Cargo.toml")
t = p.read_text(encoding="utf-8").replace(
    'members = ["rfb", "rfb-rig", "rfb-runtime", "ben"]',
    'members = ["rfb", "rfb-rig", "rfb-runtime"]')
p.write_text(t, encoding="utf-8", newline="")

# rfb-runtime：zeroboot feature/模块声明/[[test]] 段。
p = Path("rfb-runtime/Cargo.toml")
t = p.read_text(encoding="utf-8")
t = re.sub(r'\nzeroboot = \["guest"\]', "", t, count=1)
t = t.replace('cli = ["guest", "forkd", "firecracker", "zeroboot"]',
              'cli = ["guest", "forkd", "firecracker"]')
lines = t.splitlines(keepends=True)
out, i = [], 0
while i < len(lines):
    if lines[i].strip() == "[[test]]" and i + 1 < len(lines) \
            and "zeroboot" in lines[i + 1]:
        i += 3
        continue
    out.append(lines[i])
    i += 1
p.write_text("".join(out), encoding="utf-8", newline="")

p = Path("rfb-runtime/src/lib.rs")
t = p.read_text(encoding="utf-8")
lines = t.splitlines(keepends=True)
out = []
for line in lines:
    if line.strip().startswith("pub mod zeroboot_") \
            and "zeroboot_protocol" not in line:
        while out and (out[-1].strip().startswith("#[cfg(")
                       or out[-1].strip().startswith("///")):
            out.pop()
        continue
    out.append(line)
p.write_text("".join(out), encoding="utf-8", newline="")

# 测试文件的 zeroboot 用例 / 文案。
drop_block("rfb/tests/backend_factory.rs",
           "fn zeroboot_selection_reports_unmet_asset_prerequisites(")
drop_block("rfb/tests/cli_backend.rs",
           "fn zeroboot_up_rejects_bad_arguments_without_touching_kvm(")
drop_block("rfb/tests/cli_backend.rs",
           "fn preflight_reports_json_without_kvm_requirement(")
drop_block("rfb/tests/public_api_boundaries.rs",
           "mod zeroboot_public {")
p = Path("rfb/tests/cli_skills.rs")
t = p.read_text(encoding="utf-8")
p.write_text(
    "".join(l for l in t.splitlines(keepends=True) if "zeroboot verify" not in l),
    encoding="utf-8", newline="")

# image_build/all.rs：build-all 尾部的 zeroboot verify 步骤。
p = Path("rfb/src/cli/image_build/all.rs")
t = p.read_text(encoding="utf-8")
new, n = re.subn(
    r'#\[cfg\(unix\)\]\n            \{\n                let verify_report = crate::cli::zeroboot::verify\(.*?report\["verify"\] = verify_report;\n            \}\n',
    "", t, flags=re.S)
if n != 1:
    raise SystemExit("all.rs: zeroboot verify block not found")
p.write_text(new, encoding="utf-8", newline="")
print("  hook surgery done")
PY

echo "== 4. 自校验：zeroboot 引用残留（白名单，仅 .rs）=="
LEFTOVER=$(grep -rn --include='*.rs' "zeroboot" rfb/src rfb/tests 2>/dev/null \
  | grep -vE "rfb/src/cli/image_build/(build|artifact)\.rs|rfb/src/backend\.rs" \
  | grep -vE "zeroboot_protocol|zeroboot-zbrt|rfb-zeroboot|ZBRT|feature = \"zeroboot\"|crate::zeroboot::|contains\(\"zeroboot\"\)|//|zeroboot/firecracker" \
  | grep -vE "assert_eq!\((zeroboot|zbrt)\.|(zeroboot|zbrt)\.backend = |zeroboot requires|let zeroboot = |fn zeroboot_uses|zeroboot\.validate|zeroboot protocol mismatch" \
  || true)
if [ -n "$LEFTOVER" ]; then
  echo "  残留引用（需要人工裁决或补充清单）："
  echo "$LEFTOVER" | head -15
  exit 1
fi
echo "  clean"

if [ "$CHECK" = "1" ]; then
  echo "== 5. 简版编译门禁 =="
  source ~/.cargo/env 2>/dev/null || true
  cargo check --workspace --locked --no-default-features --features forkd,cli 2>&1 | tail -2
  cargo check --workspace --locked --no-default-features 2>&1 | tail -1
  echo "SIMPLE_OK"
fi
echo "SPLIT_DONE → $TARGET"
