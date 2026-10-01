#!/usr/bin/env bash
# 剥离手术：把当前 rfb 树变换为 forkd-only 简版树（确定性、可重复）。
#
#   scripts/split-minimal.sh [TARGET_DIR]      # 生成简版树（默认 ./minimal-dist）
#   scripts/split-minimal.sh --check [TARGET]  # 生成 + 编译/测试门禁验证
#
# 原则：
# - 源树（main）永远不动；简版每次从最新 main 重新生成（"同步迭代 = 重新
#   手术"——没有长期分支、没有 merge、没有冲突）。
# - 依赖结构约定（主分支纪律）：zeroboot 的命令面/实现/测试全部集中在
#   zeroboot 专属文件里；共享路由文件（commands/dispatch/mod/lib/Cargo.toml）
#   的挂接点各只有几行——手术 = 删文件 + 删这几行。
# - 简版裁剪面（三块）：
#   a) Rust 工作区去 zeroboot（provider/session/CLI 面/测试）；
#   b) 四语言 SDK 去 ZBRT 传输（只留 forkd HTTP/NDJSON；transport="zbrt"
#      一律 ValidationError fail-closed）；
#   c) 去 mlua（lua 解释器支持；python/rustpython 保留）。
# - 自校验：简版树里 zeroboot/zbrt/mlua 引用只剩白名单类，出现其他引用即失败。
# - --check 跑简版编译门禁（forkd-only feature 组合）+ SDK 测试门（python/
#   node/java；csharp 无 dotnet 环境时跳过并提示），失败退出 1——发布流水线
#   用它做门。
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
  --exclude=dist --exclude=__pycache__ --exclude=.pytest_cache \
  --exclude=minimal-dist --exclude=.github \
  .) | tar xf - -C "$TARGET"
cd "$TARGET"

echo "== 2. 删除 zeroboot / ZBRT-SDK / lua 专属文件 =="
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
  # SDK ZBRT 传输（python/node/java/csharp）+ Rust SDK 本体的 ZBRT 客户端
  rfb/src/client/zbrt.rs
  sdk/python/rfb_sdk/_zbrt.py
  sdk/python/tests/test_zbrt.py
  sdk/python/tests/test_zbrt_client.py
  sdk/python/tests/test_zbrt_codec.py
  sdk/nodejs/src/zbrt-frame.ts
  sdk/nodejs/src/zbrt-codec.ts
  sdk/nodejs/src/zbrt-connection.ts
  sdk/nodejs/src/test/zbrt.test.ts
  sdk/nodejs/src/test/zbrt-vectors.test.ts
  sdk/nodejs/src/test/zbrt-connection.test.ts
  sdk/nodejs/src/test/sandbox-zbrt.test.ts
  sdk/java/src/main/java/io/rfb/sdk/internal/ZbrtCodec.java
  sdk/java/src/main/java/io/rfb/sdk/internal/ZbrtConnection.java
  sdk/java/src/main/java/io/rfb/sdk/internal/ZbrtFrame.java
  sdk/java/tests/src/test/java/io/rfb/sdk/FakeZbrtServer.java
  sdk/java/tests/src/test/java/io/rfb/sdk/ZbrtClientTest.java
  sdk/java/tests/src/test/java/io/rfb/sdk/ZbrtCodecTest.java
  sdk/java/tests/src/test/java/io/rfb/sdk/ZbrtConnectionTest.java
  sdk/java/tests/src/test/java/io/rfb/sdk/ZbrtFrameCodecTest.java
  sdk/csharp/src/Rfb.Sdk/Internal/ZbrtTcpClient.cs
  sdk/csharp/src/Rfb.Sdk/Internal/ZbrtFrameCodec.cs
  sdk/csharp/tests/Rfb.Sdk.Tests/ZbrtClientTests.cs
  sdk/csharp/tests/Rfb.Sdk.Tests/ZbrtCodecStrictTests.cs
  sdk/csharp/tests/Rfb.Sdk.Tests/GoldenVectorTests.cs
  # lua 解释器（mlua）
  rfb-runtime/src/interpreters/lua.rs
  # 共享 zbrt 测试 fake（主树 fake 合并后置于 tests/common）
  rfb/tests/common/zbrt.rs
  # 真机 zeroboot E2E 助手（minimal 无 zeroboot）
  rfb/tests/common/realvm.rs
  rfb-runtime/tests/interpreters_lua.rs
  examples/full/sites/demo.lua
  # 构建产物残留（tar 的 ./ 前缀让 --exclude 不可靠，复制后删）
  sdk/csharp/obj
  sdk/csharp/bin
)
for path in "${REMOVE[@]}"; do
  rm -rf "$path"
  echo "  removed $path"
done
find sdk/csharp -type d \( -name obj -o -name bin \) -exec rm -rf {} + 2>/dev/null || true

echo "== 3. 挂接点行级手术（Rust：zeroboot）=="
python3 - <<'PY'
import re
from pathlib import Path


def drop_lines(path, matchers):
    """按谓词删行；matcher(line)->True 的行（连同其紧邻的前导 #[...]//// 行）
    被删除。返回删除数。"""
    p = Path(path)
    lines = p.read_text(encoding="utf-8").splitlines(keepends=True)
    out, removed = [], 0
    for line in lines:
        if any(m(line) for m in matchers):
            while out and (out[-1].strip().startswith("#[")
                           or out[-1].strip().startswith("///")):
                out.pop()
            removed += 1
            continue
        out.append(line)
    p.write_text("".join(out), encoding="utf-8", newline="")
    return removed


def drop_block(path, start_marker):
    """删除 start_marker 所在行起、大括号平衡的块（含前导属性行；`{` 可在
    下一行：Allman 风格同样处理）。"""
    p = Path(path)
    lines = p.read_text(encoding="utf-8").splitlines(keepends=True)
    out, i, removed = [], 0, 0
    while i < len(lines):
        line = lines[i]
        if start_marker in line:
            while out and (out[-1].strip().startswith("#[")
                           or out[-1].strip().startswith("///")):
                out.pop()
            j = i
            while "{" not in lines[j]:
                j += 1
            depth = lines[j].count("{") - lines[j].count("}")
            j += 1
            while j < len(lines) and depth > 0:
                depth += lines[j].count("{") - lines[j].count("}")
                j += 1
            removed += 1
            i = j
            continue
        out.append(line)
        i += 1
    if removed == 0:
        print(f"  [skip] {path}: block {start_marker!r} already absent")
        return
    p.write_text("".join(out), encoding="utf-8", newline="")


# rfb/src/cli/mod.rs：zeroboot 模块注册行。
drop_lines("rfb/src/cli/mod.rs",
           [lambda l: l.strip() in ("pub mod zeroboot;", "pub mod zeroboot_backend;")])

# rfb/src/lib.rs：zeroboot 家族声明的手术。
# - protocol：共享 wire（RFB1/SDK 都引用）→ 去 zeroboot feature 门，保留。
# - firecracker：lib.rs 里现在是内联 re-export 块（pub use
#   rfb_runtime::firecracker_core::firecracker::*；模块本体在 rfb-runtime），
#   其 zeroboot → forkd 的门改写在 §3c 统一做（forkd feature 同时挂上
#   rfb-runtime/firecracker，见 rfb/Cargo.toml 的 forkd features 改写）。
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
# firecracker 模块的门是 forkd，而它的 attach_pdeathsig 用 libc——forkd 必须自带。
t = t.replace('forkd = ["dep:reqwest", "dep:tokio", "dep:url"]',
              'forkd = ["dep:reqwest", "dep:tokio", "dep:url", "dep:libc"]')
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

# rfb-runtime：zeroboot/interpreters_lua 的 [[test]] 段 + feature/模块声明。
p = Path("rfb-runtime/Cargo.toml")
t = p.read_text(encoding="utf-8")
t = re.sub(r'\nzeroboot = \["guest"\]', "", t, count=1)
t = t.replace('cli = ["guest", "forkd", "firecracker", "zeroboot"]',
              'cli = ["guest", "forkd", "firecracker"]')
lines = t.splitlines(keepends=True)
out, i = [], 0
while i < len(lines):
    if lines[i].strip() == "[[test]]" and i + 1 < len(lines) \
            and ("zeroboot" in lines[i + 1] or "interpreters_lua" in lines[i + 1]):
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
t = t.replace('''    assert!(
        content.content.contains("zeroboot verify"),
        "runbook must reference the verify subcommand"
    );
''', "")
assert "zeroboot verify" not in t, "cli_skills.rs: zeroboot verify leftover"
p.write_text(t, encoding="utf-8", newline="")

# image_build/all.rs：build-all 尾部的 zeroboot verify 步骤。
p = Path("rfb/src/cli/image_build/all.rs")
t = p.read_text(encoding="utf-8")
new, n = re.subn(
    r'#\[cfg\(unix\)\]\n            \{\n                let verify_report = crate::cli::zeroboot::verify\(.*?report\["verify"\] = verify_report;\n            \}\n',
    "", t, flags=re.S)
if n != 1:
    raise SystemExit("all.rs: zeroboot verify block not found")
p.write_text(new, encoding="utf-8", newline="")
print("  zeroboot hooks done")
PY

echo "== 3b. SDK 手术：四语言去 ZBRT 传输 =="
python3 - <<'PY'
import re
from pathlib import Path


def must_replace(path, old, new, count=1):
    p = Path(path)
    t = p.read_text(encoding="utf-8")
    if t.count(old) != count:
        raise SystemExit(f"{path}: pattern {old[:60]!r} count={t.count(old)} expected {count}")
    p.write_text(t.replace(old, new), encoding="utf-8", newline="")


def drop_brace_block(path, marker, expect_min=1):
    """删除所有含 marker 的行起、大括号平衡的块（`{` 可在下一行：Allman）。
    marker 到 `{` 之间遇 `;` 结尾的行 = 无块的单条语句，只删到该行。
    块后若紧跟 else（if/else 结构）则拒绝——需人工手术。"""
    p = Path(path)
    lines = p.read_text(encoding="utf-8").splitlines(keepends=True)
    out, i, removed = [], 0, 0
    while i < len(lines):
        line = lines[i]
        if marker in line:
            while out and (out[-1].strip().startswith(("#[", "///", "//", "*", "/*"))
                           or out[-1].strip().startswith("*")):
                out.pop()
            j = i
            found = False
            while j < len(lines):
                l = lines[j]
                if "{" in l:
                    found = True
                    break
                if l.rstrip().endswith(";"):
                    j += 1
                    break
                j += 1
            if found:
                depth = lines[j].count("{") - lines[j].count("}")
                j += 1
                while j < len(lines) and depth > 0:
                    depth += lines[j].count("{") - lines[j].count("}")
                    j += 1
            if j < len(lines) and re.match(r"\s*(else\b|} else)", lines[j]):
                raise SystemExit(f"{path}:{j+1}: else after dropped block — manual surgery needed")
            removed += 1
            i = j
            continue
        out.append(line)
        i += 1
    if removed == 0:
        print(f"  [skip] {path}: {marker!r} already absent")
        p.write_text("".join(out), encoding="utf-8", newline="")
        return
    if removed < expect_min:
        print(f"  [warn] {path}: {marker!r} matched {removed} (< {expect_min})")
        raise SystemExit(f"{path}: marker {marker!r} matched {removed} blocks (< {expect_min})")
    p.write_text("".join(out), encoding="utf-8", newline="")


def drop_py_block(path, marker, expect_min=1):
    """删除所有含 marker 的（非注释）行起、缩进更深的整块（Python）。"""
    p = Path(path)
    lines = p.read_text(encoding="utf-8").splitlines(keepends=True)
    out, i, removed = [], 0, 0
    while i < len(lines):
        line = lines[i]
        if marker in line and not line.lstrip().startswith("#"):
            base = len(line) - len(line.lstrip())
            i += 1
            while i < len(lines):
                l = lines[i]
                if l.strip() == "":
                    i += 1
                    continue
                if len(l) - len(l.lstrip()) > base:
                    i += 1
                    continue
                break
            removed += 1
            continue
        out.append(line)
        i += 1
    if removed == 0:
        print(f"  [skip] {path}: {marker!r} already absent")
        p.write_text("".join(out), encoding="utf-8", newline="")
        return
    if removed < expect_min:
        print(f"  [warn] {path}: {marker!r} matched {removed} (< {expect_min})")
        raise SystemExit(f"{path}: {marker!r} matched {removed} (< {expect_min})")
    p.write_text("".join(out), encoding="utf-8", newline="")


# ---------- python ----------
S = "sdk/python"
# _parse_host_port 搬进 _guest.py（NDJSON 也依赖它），随后 _zbrt.py 已删。
guest = Path(f"{S}/rfb_sdk/_guest.py")
t = guest.read_text(encoding="utf-8")
t = t.replace("from ._zbrt import _parse_host_port\n", "")
assert "_parse_host_port" in t, "_guest.py still uses _parse_host_port"
t += '''

def _parse_host_port(address: str) -> tuple:
    """Parse ``host:port`` into its parts (NDJSON guest endpoints)."""
    host, sep, port_text = address.rpartition(":")
    if not sep or not host:
        raise DecodeError(f"invalid guest address: {address!r}")
    try:
        port = int(port_text)
    except ValueError as e:
        raise DecodeError(f"invalid guest address: {address!r}") from e
    return host, port
'''
guest.write_text(t, encoding="utf-8", newline="")

facade = f"{S}/rfb_sdk/facade.py"
must_replace(facade, '''from ._zbrt import (
    FS_OP_FIND,
    FS_OP_GREP,
    FS_OP_LS,
    FS_OP_READ,
    FS_OP_WRITE,
    _ZbrtGuestClient,
)
''', "")
must_replace(facade, "    MAX_ZBRT_PAYLOAD_BYTES,\n", "")
must_replace(facade, "    validate_zbrt_args,\n", "")
must_replace(facade, '''DEFAULT_ZBRT_TCP = "127.0.0.1:15000"
TRANSPORT_NDJSON = "ndjson"
TRANSPORT_ZBRT = "zbrt"
''', '''TRANSPORT_NDJSON = "ndjson"
''')
must_replace(facade, '''def _zbrt_exec_result(t: tuple) -> ExecResult:
    code, stdout, stderr = t
    return ExecResult(exit_code=code, stdout=stdout, stderr=stderr, timed_out=False)


''', "")
must_replace(facade, '''        if self._guest_client_cache is None:
            self._guest_client_cache = (
                _ZbrtGuestClient(self.guest_addr, timeout_s)
                if self._transport == "zbrt"
                else _GuestNdjsonClient(self.guest_addr, timeout_s))
        return self._guest_client_cache
''', '''        return _GuestNdjsonClient(self.guest_addr, timeout_s)
''')
must_replace(facade, '''        ZBRT carries ``args`` as-is; the NDJSON action dict drops None values
        (absent keys on the wire) to mirror the historical request shape.
        """
        guest = self._guest()
        if self._transport == "zbrt":
            return guest.fs_op(op, path, args)
        request''', '''        The NDJSON action dict drops None values (absent keys on the wire) to
        mirror the historical request shape.
        """
        guest = self._guest()
        request''')
must_replace(facade, "def _fs_call(self, op: int, action: str, path: str, args: dict) -> dict:",
             "def _fs_call(self, action: str, path: str, args: dict) -> dict:")
p = Path(facade)
t = p.read_text(encoding="utf-8")
t = re.sub(r"self\._fs_call\(\s*FS_OP_\w+,\s*", "self._fs_call(", t)
p.write_text(t, encoding="utf-8", newline="")
must_replace(facade, '''        if self._transport == "zbrt":
            return guest.ping()
''', "")
must_replace(facade, 'if self._transport != "zbrt" and stdin:', "if stdin:")
must_replace(facade, '''        if self._transport == "zbrt":
            validate_zbrt_args(args)
            validate_payload_size(len(stdin), MAX_ZBRT_PAYLOAD_BYTES)
            return _zbrt_exec_result(guest.exec(args, cwd, timeout_s, stdin))
''', "")
p = Path(facade)
t = p.read_text(encoding="utf-8")
new, n = re.subn(
    r'        if self\._transport == "zbrt":\n.*?        value = guest\.eval\(code, cwd, timeout_s\)',
    '        value = guest.eval(code, cwd, timeout_s)', t, count=1, flags=re.S)
if n != 1:
    raise SystemExit("facade.py: eval zbrt branch not found")
t = new
new, n = re.subn(
    r'        guest = self\._guest\(\)\n        if self\._transport == "zbrt":\n.*?        else:\n            inner = guest\.stream\(args, cwd, pty, env\)\n',
    '        guest = self._guest()\n        inner = guest.stream(args, cwd, pty, env)\n', t, count=1, flags=re.S)
if n != 1:
    raise SystemExit("facade.py: stream zbrt branch not found")
t = new
t = t.replace('"""Open an interactive stream (transport per the handle; ZBRT has no pty/stdin)."""',
              '"""Open an interactive stream."""')
p.write_text(t, encoding="utf-8", newline="")
must_replace(facade, '"stdin is only supported over the ZBRT transport"',
             '"stdin is not supported over the ndjson transport"')
assert "zbrt" not in Path(facade).read_text(encoding="utf-8").lower(), "facade.py zbrt leftover"

init_py = f"{S}/rfb_sdk/__init__.py"
p = Path(init_py)
t = p.read_text(encoding="utf-8")
t = t.replace("protocol adapters (forkd controller HTTP, forkd guest NDJSON, ZBRT v1 frames)",
              "protocol adapters (forkd controller HTTP, forkd guest NDJSON)")
# host.py 新增的 ZerobootHost（直连 Firecracker + ZBRT 客户端工厂）——minimal
# 无 zeroboot：整类删，ForkdHost/TcpVsockRelay/boot_firecracker 保留。
t = t.replace("from .host import ForkdHost, TcpVsockRelay, ZerobootHost\n",
              "from .host import ForkdHost, TcpVsockRelay\n")
t = t.replace('    "ZerobootHost",\n', "")
t = t.replace("    DEFAULT_ZBRT_TCP,\n    TRANSPORT_NDJSON,\n    TRANSPORT_ZBRT,\n",
              "    TRANSPORT_NDJSON,\n")
t = t.replace('    "DEFAULT_ZBRT_TCP",\n    "TRANSPORT_NDJSON",\n    "TRANSPORT_ZBRT",\n',
              '    "TRANSPORT_NDJSON",\n')
assert "DEFAULT_ZBRT_TCP" not in t and "TRANSPORT_ZBRT" not in t, \
    "__init__.py: zbrt-const leftover"
assert "ZerobootHost" not in t, "__init__.py: ZerobootHost leftover"
p.write_text(t, encoding="utf-8", newline="")

host_py = f"{S}/rfb_sdk/host.py"
must_replace(host_py, '''- :class:`ZerobootHost` boots one Firecracker microVM directly (the UDS
  management API), bridges its vsock relay UDS onto TCP (Firecracker's
  ``CONNECT <guest-port>`` preamble protocol), and hands out a ZBRT client.
- :class:`ForkdHost` spawns''',
             '''- :class:`ForkdHost` spawns''')
must_replace(host_py, "from ._zbrt import _ZbrtGuestClient as _ZbrtClient\n", "")
must_replace(host_py, "from .models import SandboxInfo\n", "")
must_replace(host_py, "from .facade import RfbClient, Sandbox\n",
             "from .facade import RfbClient\n")
must_replace(host_py,
             '__all__ = ["ForkdHost", "TcpVsockRelay", "ZerobootHost", "boot_firecracker"]',
             '__all__ = ["ForkdHost", "TcpVsockRelay", "boot_firecracker"]')
must_replace(host_py, '''# ---------------------------------------------------------------------------
# Zeroboot host: one VM + TCP bridge, ZBRT clients direct
# ---------------------------------------------------------------------------


''', "")
drop_py_block(host_py, "class ZerobootHost:")
assert "zbrt" not in Path(host_py).read_text(encoding="utf-8").lower(), \
    "host.py zbrt leftover"
assert "zeroboot" not in Path(host_py).read_text(encoding="utf-8").lower(), \
    "host.py zeroboot leftover"
errors_py = f"{S}/rfb_sdk/errors.py"
p = Path(errors_py)
t = p.read_text(encoding="utf-8")
t = t.replace("guest error line, ZBRT Error frame, forkd error field",
              "guest error line, forkd error field")
p.write_text(t, encoding="utf-8", newline="")

validation = f"{S}/rfb_sdk/validation.py"
must_replace(validation, '''# ZBRT v1 caps: argc fits one header byte, payloads are u32-bounded.
MAX_ZBRT_ARGC = 255
MAX_ZBRT_PAYLOAD_BYTES = 16 * 1024 * 1024
''', "")
must_replace(validation, '''def validate_zbrt_args(args) -> None:
    """ZBRT v1 encodes argc in one byte; reject locally instead of leaking a
    ValueError after the connection is already open."""
    if len(args) > MAX_ZBRT_ARGC:
        raise ValidationError(f"argv exceeds the {MAX_ZBRT_ARGC}-argument ZBRT limit")


''', "")
must_replace(validation, '''    if transport not in ("ndjson", "zbrt"):
        raise ValidationError("transport must be 'ndjson' or 'zbrt'")''',
             '''    if transport != "ndjson":
        raise ValidationError("transport must be 'ndjson'")''')

# python 测试
tf = f"{S}/tests/test_facade.py"
must_replace(tf, "from tests.fake_servers import FakeControllerServer, FakeNdjsonGuestServer, FakeZbrtServer",
             "from tests.fake_servers import FakeControllerServer, FakeNdjsonGuestServer")
must_replace(tf, '''BOTH transports (ndjson default and zbrt) with identical result shapes.''',
             '''the ndjson transport (forkd-only minimal build).''')
must_replace(tf, '''    def test_exec_with_stdin_and_timeout(self):
        sandbox = self._sandbox()
        if self.transport != "zbrt":
            # The NDJSON wire has no exec stdin channel: non-empty stdin
            # fails closed (running the command without its input would be
            # silent data loss).
            with self.assertRaises(ValidationError):
                sandbox.exec(["cat"], timeout_s=5.0, stdin=b"xyz")
            return
        result = sandbox.exec(["cat"], timeout_s=5.0, stdin=b"xyz")
        self.assertEqual(result.exit_code, 0)
''', '''    def test_exec_stdin_fails_closed(self):
        sandbox = self._sandbox()
        # The NDJSON wire has no exec stdin channel: non-empty stdin
        # fails closed (running the command without its input would be
        # silent data loss).
        with self.assertRaises(ValidationError):
            sandbox.exec(["cat"], timeout_s=5.0, stdin=b"xyz")
''')
must_replace(tf, '''        if self.transport == "zbrt":
            # ZBRT v1 has no eval opcode and the reference guest maps Execute
            # verbatim onto `exec`: the facade fails closed locally instead of
            # running a literal `eval <code>` command.
            with self.assertRaises(ValidationError):
                sandbox.eval("print(42)")
            return
        result = sandbox.eval("print(42)")''', '''        result = sandbox.eval("print(42)")''')
drop_py_block(tf, "class FacadeZbrtTests")
drop_py_block(tf, "def test_connect_preserves_sandbox_transport")
drop_py_block(f"{S}/tests/fake_servers.py", "class FakeZbrtServer")
print("  python done")

# ---------- nodejs ----------
N = "sdk/nodejs"
must_replace(f"{N}/src/sandbox.ts", "import { ZbrtConnection } from './zbrt-connection.js';\n", "")
must_replace(f"{N}/src/sandbox.ts",
             '''/** Default ZBRT bridge TCP endpoint (RFB_ZBRT_TCP default). */
export const DEFAULT_ZBRT_TCP = '127.0.0.1:15000';
''', "")
drop_brace_block(f"{N}/src/sandbox.ts", "async #borrowExecConn(")
drop_brace_block(f"{N}/src/sandbox.ts", "async #withExecConn<")
must_replace(f"{N}/src/sandbox.ts", '''  /** exec 温连接池：已 Hello 的空闲连接，借还复用（空闲 >1s 才验活）。 */
  #zbrtExecPool: { conn: ZbrtConnection; lastUsed: number }[] = [];
''', "")
must_replace(f"{N}/src/sandbox.ts",
             "    for (const { conn } of this.#zbrtExecPool.splice(0)) conn.close();\n", "")
drop_brace_block(f"{N}/src/sandbox.ts", "export const TRANSPORT_ZBRT")
drop_brace_block(f"{N}/src/sandbox.ts", "function zbrtTimeoutMs")
drop_brace_block(f"{N}/src/sandbox.ts", "this.transport === TRANSPORT_ZBRT", expect_min=6)
must_replace(f"{N}/src/sandbox.ts", "'stdin is only supported over the ZBRT transport'",
             "'stdin is not supported over the ndjson transport'")
drop_brace_block(f"{N}/src/sandbox.ts", "async #zbrt(")
drop_brace_block(f"{N}/src/sandbox.ts", "class ZbrtGuestStream")
# validation.ts：transport 校验收口 + ZBRT 上限/zbrtArgs 删除（zbrt 的调用
# 点都在 sandbox.ts 的 zbrt 分支里，随分支一起删）。
V = f"{N}/src/validation.ts"
must_replace(V, "export const TRANSPORT_ZBRT = 'zbrt';\n", "")
must_replace(V, '''// ZBRT v1 caps: argc fits one header byte, payloads are u32-bounded and the
// reference guest enforces a 16 MiB cap on every frame payload (§8).
export const MAX_ZBRT_ARGC = 255;
export const MAX_ZBRT_PAYLOAD_BYTES = 16 * 1024 * 1024;
''', "")
must_replace(V, "  if (value !== TRANSPORT_NDJSON && value !== TRANSPORT_ZBRT) {",
             "  if (value !== TRANSPORT_NDJSON) {")
drop_brace_block(V, "export function zbrtArgs(")
must_replace(f"{N}/src/client.ts",
             "import { Sandbox, TRANSPORT_NDJSON, TRANSPORT_ZBRT } from './sandbox.js';",
             "import { Sandbox, TRANSPORT_NDJSON } from './sandbox.js';")
must_replace(f"{N}/package.json",
             'node dist/test/validation.test.js && node dist/test/zbrt.test.js && node dist/test/zbrt-vectors.test.js && node dist/test/zbrt-connection.test.js && node dist/test/client.test.js && node dist/test/sandbox-zbrt.test.js && node dist/test/sandbox-ndjson.test.js && node dist/test/fs-actions.test.js && node dist/test/ndjson-stream.test.js',
             'node dist/test/validation.test.js && node dist/test/client.test.js && node dist/test/sandbox-ndjson.test.js && node dist/test/fs-actions.test.js && node dist/test/ndjson-stream.test.js')
must_replace(f"{N}/package.json",
             "(TCP NDJSON + ZBRT v1 binary frames)", "(TCP NDJSON)")
# client.test.ts：connect(sandbox, transport) 覆盖测试里的 zbrt 覆盖断言整行
# 删（minimal 里 zbrt 一律 fail-closed；同用例的 'grpc' 拒绝断言已覆盖校验，
# 且 node 的 src/test 在残留扫描范围内，不能留 'zbrt' 字面量）。
must_replace(f"{N}/src/test/client.test.ts",
             "    assert.equal((await client.connect(handle, 'zbrt')).transport, 'zbrt');\n",
             "")
drop_brace_block(f"{N}/src/test/client.test.ts", "zbrt stream rejects pty before connecting")
drop_brace_block(f"{N}/src/test/client.test.ts", "zbrt stream rejects env before connecting")
print("  nodejs done")

# ---------- java ----------
J = "sdk/java"
must_replace(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java",
             "import io.rfb.sdk.internal.ZbrtConnection;\n", "")
must_replace(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java",
             'if (!RfbClient.TRANSPORT_NDJSON.equals(transport) && !RfbClient.TRANSPORT_ZBRT.equals(transport)) {',
             'if (!RfbClient.TRANSPORT_NDJSON.equals(transport)) {')
must_replace(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java",
             'throw new ValidationError("transport must be \\"ndjson\\" or \\"zbrt\\"");',
             'throw new ValidationError("transport must be \\"ndjson\\"");')
drop_brace_block(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java",
                 "RfbClient.TRANSPORT_ZBRT.equals(transport)", expect_min=7)
must_replace(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java",
             'throw new ValidationError("stdin is only supported over the ZBRT transport");',
             'throw new ValidationError("stdin is not supported over the ndjson transport");')
drop_brace_block(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java", "private JsonNode zbrtFs(")
drop_brace_block(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java", "private ZbrtConnection openZbrt(")
drop_brace_block(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java", "private <T> T zbrtControl(")
drop_brace_block(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java", "private static boolean zbrtRetryable(")
drop_brace_block(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java", "private ExecResult execViaPool(")
drop_brace_block(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java", "private ZbrtConnection borrowExecConn(")
drop_brace_block(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java", "private void repayExecConn(")
must_replace(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java", '''    /** exec 温连接池（已 Hello 的空闲连接；空闲 >1s 借出时才验活）。 */
    private final java.util.concurrent.ConcurrentLinkedQueue<
            java.util.AbstractMap.SimpleEntry<ZbrtConnection, Long>>
            zbrtExecPool = new java.util.concurrent.ConcurrentLinkedQueue<>();
''', "")
must_replace(f"{J}/src/main/java/io/rfb/sdk/Sandbox.java",
             '''        java.util.AbstractMap.SimpleEntry<ZbrtConnection, Long> pooled;
        while ((pooled = zbrtExecPool.poll()) != null) {
            pooled.getKey().close();
        }
''', "")


# GuestStream：整文件改写为 ndjson-only（小文件，重写最干净）。
Path(f"{J}/src/main/java/io/rfb/sdk/GuestStream.java").write_text(
    """package io.rfb.sdk;

import com.fasterxml.jackson.databind.JsonNode;
import io.rfb.sdk.internal.GuestNdjsonStream;
import io.rfb.sdk.internal.Json;

/**
 * Interactive guest stream (UNIFIED_API.md §5): read events with
 * {@link #nextEvent()} until the terminal {@code exit} event, send input with
 * {@link #sendInput(String)} and request termination with the idempotent
 * {@link #stop()}.
 */
public final class GuestStream implements AutoCloseable {
    private final GuestNdjsonStream ndjson;

    private GuestStream(GuestNdjsonStream ndjson) {
        this.ndjson = ndjson;
    }

    static GuestStream overNdjson(GuestNdjsonStream stream) {
        return new GuestStream(stream);
    }

    /**
     * Read the next event. Returns null on clean close. The terminal
     * {@code exit} event (with code, possibly null) is returned normally.
     */
    public StreamEvent nextEvent() {
        JsonNode node = ndjson.nextEvent();
        return node == null ? null : mapNdjsonEvent(node);
    }

    /**
     * Send input to the running stream. Raises {@link RemoteError} after the
     * stream terminated or was stopped.
     */
    public void sendInput(String text) {
        ndjson.sendInput(text);
    }

    /** Request termination; idempotent and safe after the exit event. */
    public void stop() {
        ndjson.stop();
    }

    @Override
    public void close() {
        ndjson.close();
    }

    /**
     * NDJSON event mapping (mirrors the Rust {@code forkd_stream_event}):
     * started → stdout → stderr → exit ordering per detection keys.
     */
    static StreamEvent mapNdjsonEvent(JsonNode value) {
        if (isStarted(value)) {
            return StreamEvent.started();
        }
        JsonNode code = value.get("exit_code");
        if (code != null && code.isNumber()) {
            return StreamEvent.exit(code.intValue());
        }
        if (value.path("done").asBoolean(false)) {
            return StreamEvent.exit(null);
        }
        if (value.has("stdout") || value.has("out")) {
            return StreamEvent.stdout(Json.valueBytes(firstOf(value, "stdout", "out")));
        }
        if (value.has("stderr") || value.has("err")) {
            return StreamEvent.stderr(Json.valueBytes(firstOf(value, "stderr", "err")));
        }
        throw new DecodeError("invalid guest stream event");
    }

    private static boolean isStarted(JsonNode value) {
        return value.path("started").asBoolean(false)
                || "started".equals(value.path("stream").asText(null))
                || "started".equals(value.path("event").asText(null));
    }

    private static JsonNode firstOf(JsonNode value, String a, String b) {
        JsonNode first = value.get(a);
        return first != null ? first : value.get(b);
    }
}
""", encoding="utf-8")

p = Path(f"{J}/src/main/java/io/rfb/sdk/RfbClient.java")
t = p.read_text(encoding="utf-8")
t = t.replace(
    '/** Transport choice for {@link #connect}: NDJSON (default) or ZBRT v1 frames. */',
    '/** Transport choice for {@link #connect}: NDJSON. */')
# TRANSPORT_ZBRT 常量连同其文档行一起删（只删 const 行会留下悬空 javadoc）。
t = t.replace(
    '    /** ZBRT (ZeroBoot v1) guest transport selector for {@link #connect}. */\n'
    '    public static final String TRANSPORT_ZBRT = "zbrt";\n', "")
t = t.replace(
    '\n    /** Default ZBRT bridge TCP endpoint (RFB_ZBRT_TCP default). */\n'
    '    public static final String DEFAULT_ZBRT_TCP = "127.0.0.1:15000";\n', "\n")
t = t.replace('!TRANSPORT_NDJSON.equals(transport) && !TRANSPORT_ZBRT.equals(transport)',
              '!TRANSPORT_NDJSON.equals(transport)')
t = t.replace('throw new ValidationError("transport must be \\"ndjson\\" or \\"zbrt\\"");',
              'throw new ValidationError("transport must be \\"ndjson\\"");')
assert "ZBRT" not in t, f"RfbClient.java: ZBRT leftover"
p.write_text(t, encoding="utf-8", newline="")

# internal/Validation.java：ZBRT 上限常量与 zbrtArgs 校验器删除（调用点都在
# Sandbox.java 的 zbrt 分支里，随分支一起删）。
JVAL = f"{J}/src/main/java/io/rfb/sdk/internal/Validation.java"
must_replace(JVAL, '''    /** ZBRT v1 caps: argc fits one header byte, payloads are u32-bounded. */
    public static final int MAX_ZBRT_ARGC = 255;
    public static final int MAX_ZBRT_PAYLOAD_BYTES = 16 * 1024 * 1024;
''', "")
drop_brace_block(JVAL, "public static void zbrtArgs(")

ft = f"{J}/tests/src/test/java/io/rfb/sdk/RfbClientFacadeTest.java"
p = Path(ft)
t = p.read_text(encoding="utf-8")
t = t.replace("RfbClient.TRANSPORT_ZBRT", "RfbClient.TRANSPORT_NDJSON")
assert "ZBRT" not in t, f"{ft}: ZBRT leftover"
p.write_text(t, encoding="utf-8", newline="")
print("  java done")

# ---------- csharp ----------
C = "sdk/csharp"
sb = f"{C}/src/Rfb.Sdk/Sandbox.cs"
# Sandbox.Attach（直连 guest 地址的入口）：与构造器同语义收口成 ndjson-only。
must_replace(sb, 'if (transport != RfbClient.TransportNdjson && transport != RfbClient.TransportZbrt)',
             'if (transport != RfbClient.TransportNdjson)')
must_replace(sb, 'if (transport is not ("ndjson" or "zbrt"))', 'if (transport != "ndjson")')
must_replace(sb, 'throw new ValidationException("transport must be \\"ndjson\\" or \\"zbrt\\"");',
             'throw new ValidationException("transport must be \\"ndjson\\"");')
must_replace(sb, 'throw new ValidationException("stdin is only supported over the ZBRT transport");',
             'throw new ValidationException("stdin is not supported over the ndjson transport");')
# GuestValidation.Transport：简版 ndjson-only（Connect 的流量前校验也要收口）。
must_replace(f"{C}/src/Rfb.Sdk/Internal/GuestValidation.cs",
             'if (transport is null || transport == "ndjson" || transport == "zbrt")',
             'if (transport is null || transport == "ndjson")')
must_replace(f"{C}/src/Rfb.Sdk/Internal/GuestValidation.cs",
             'throw new ValidationException("transport must be \\"ndjson\\" or \\"zbrt\\"");',
             'throw new ValidationException("transport must be \\"ndjson\\"");')
# GuestValidation：ZBRT 上限常量与 ZbrtArgc 校验器删除（调用点都在 Sandbox.cs
# 的 zbrt 尾巴里，随 unwrap_ndjson_if 一起删）。
gv = f"{C}/src/Rfb.Sdk/Internal/GuestValidation.cs"
must_replace(gv, '''    // ZBRT v1 caps: argc fits one header byte, payloads are u32-bounded
    // (PROTOCOL.md §3.2; rfb/src/guest/limits.rs).
    public const int MaxZbrtArgc = 255;
    public const int MaxZbrtPayloadBytes = 16 * 1024 * 1024; // 16 MiB
''', "")
drop_brace_block(gv, "public static void ZbrtArgc(")
# RfbClient：TransportZbrt 常量连同文档行删除（Attach 改写后唯一调用点消失）。
must_replace(f"{C}/src/Rfb.Sdk/RfbClient.cs",
             '''    /// <summary>Guest transport: ZBRT v1 frames.</summary>
    public const string TransportZbrt = "zbrt";
''', "")
must_replace(f"{C}/src/Rfb.Sdk/RfbClient.cs",
             '''
    /// <summary>Default ZBRT bridge TCP endpoint (RFB_ZBRT_TCP default).</summary>
    public const string DefaultZbrtTcp = "127.0.0.1:15000";
''', "")
drop_lines_csharp = [
    "    private readonly Lazy<ZbrtPool> _zbrt;",
    "        _zbrt = new Lazy<ZbrtPool>(() => new ZbrtPool(info.GuestAddr, timeout));",
]
p = Path(sb)
t = p.read_text(encoding="utf-8")
for line in drop_lines_csharp:
    assert line in t, f"Sandbox.cs missing {line!r}"
    t = t.replace(line + "\n", "")
new, n = re.subn(r'\n    /// <summary>Guest transport in use[^<]*</summary>\n', "\n", t)
t = new
new, n = re.subn(r'\n    private ZbrtPool Zbrt => Transport == "zbrt"\n        \? _zbrt\.Value\n        : throw new InvalidOperationException\("zbrt transport not active"\);\n',
                 "\n", t)
if n != 1:
    raise SystemExit(f"Sandbox.cs: Zbrt property match n={n}")
t = new
p.write_text(t, encoding="utf-8", newline="")


def unwrap_ndjson_if(path):
    """把 `if (Transport == "ndjson") { BODY }` 解包为 BODY（保留），并删除
    其后到方法结束（缩进小于 if 的 `}`）之前的全部 zbrt 尾巴（minimal 里
    transport 恒为 ndjson，构造器已 fail-closed）。"""
    p = Path(path)
    lines = p.read_text(encoding="utf-8").splitlines(keepends=True)
    out, i, removed = [], 0, 0
    while i < len(lines):
        line = lines[i]
        if 'if (Transport == "ndjson")' in line:
            indent = len(line) - len(line.lstrip())
            j = i + 1
            while "{" not in lines[j]:
                j += 1
            j += 1
            depth = 1
            while depth > 0:
                depth += lines[j].count("{") - lines[j].count("}")
                if depth == 0:
                    j += 1
                    break
                out.append(lines[j])
                j += 1
            # 删除 zbrt 尾巴直到方法结束（缩进小于 if 的 `}` = 外层块结束）
            while j < len(lines):
                l = lines[j]
                if l.strip() == "}" and (len(l) - len(l.lstrip())) < indent:
                    break
                j += 1
            removed += 1
            i = j
            continue
        out.append(line)
        i += 1
    if removed == 0:
        print(f"  [skip] {path}: ndjson unwrap already applied")
        return
    p.write_text("".join(out), encoding="utf-8", newline="")


unwrap_ndjson_if(sb)
drop_brace_block(sb, "private async Task<IReadOnlyList<string>> FindZbrt(")
drop_brace_block(sb, "private async Task<IReadOnlyList<GrepMatch>> GrepZbrt(")
assert "Zbrt" not in Path(sb).read_text(encoding="utf-8"), f"sb: Zbrt leftover"

gs = f"{C}/src/Rfb.Sdk/GuestStream.cs"
Path(gs).write_text(
    """using Rfb.Sdk.Internal;

namespace Rfb.Sdk;

/// <summary>
/// Interactive guest stream (UNIFIED_API.md §5). Events: started | stdout |
/// stderr | exit. Clean close → NextEvent returns null.
/// </summary>
public sealed class GuestStream : IDisposable
{
    private readonly ForkdGuestNdjsonStream _ndjson;

    internal GuestStream(ForkdGuestNdjsonStream session) => _ndjson = session;

    /// <summary>Release the underlying transport socket (idempotent).</summary>
    public void Dispose() => _ndjson.Dispose();

    /// <summary>Next stream event; null when the stream closed cleanly.</summary>
    public async Task<StreamEvent?> NextEvent()
    {
        var value = await _ndjson.NextEventAsync().ConfigureAwait(false);
        return value is null ? null : GuestResults.MapStreamEvent(value.Value);
    }

    /// <summary>Send stdin text; calling after the terminal event raises RemoteException.</summary>
    public async Task SendInput(string text)
    {
        await _ndjson.SendInputAsync(text).ConfigureAwait(false);
    }

    /// <summary>Request termination; idempotent.</summary>
    public async Task Stop()
    {
        await _ndjson.StopAsync().ConfigureAwait(false);
    }
}
""", encoding="utf-8")

drop_brace_block(f"{C}/tests/Rfb.Sdk.Tests/Fakes.cs", "class FakeZbrtServer")
must_replace(f"{C}/src/Rfb.Sdk/Rfb.Sdk.csproj",
    "<Description>RFB unified SDK (C#): RfbClient facade over forkd controller, forkd guest and ZBRT v1 — mirror port of the Rust reference implementation</Description>",
    "<Description>RFB minimal SDK (C#): RfbClient facade over the forkd controller and forkd guest NDJSON — forkd-only build</Description>")
must_replace(f"{C}/src/Rfb.Sdk/Rfb.Sdk.csproj",
    "<PackageTags>rfb;sandbox;forkd;firecracker;microvm;zbrt</PackageTags>",
    "<PackageTags>rfb;sandbox;forkd;firecracker;microvm</PackageTags>")
ft = f"{C}/tests/Rfb.Sdk.Tests/FacadeTests.cs"
p = Path(ft)
t = p.read_text(encoding="utf-8")
t = t.replace('        if (transport == "ndjson") {\n            var g = new FakeNdjsonGuest();\n            guest = g;\n            guestAddr = g.Address;\n        } else {\n            var g = new FakeZbrtServer();\n            guest = g;\n            guestAddr = g.Address;\n        }\n',
              '        var g = new FakeNdjsonGuest();\n        guest = g;\n        guestAddr = g.Address;\n')
t = t.replace('    [InlineData("zbrt")]\n', "")
# Connect_ExplicitTransport_OverridesHandle：zbrt 覆盖分支改为 fail-closed
# 断言（minimal 里显式 zbrt 在流量前抛 ValidationException）。
t = t.replace('''        var overridden = await fx.Client.Connect(fx.Sandbox, "zbrt");
        Assert.Equal("zbrt", overridden.Transport);
        Assert.Equal("sb-1", overridden.Id);
''', '        await Assert.ThrowsAsync<ValidationException>(() => fx.Client.Connect(fx.Sandbox, "zbrt"));\n')
assert 'Assert.Equal("zbrt", overridden.Transport)' not in t, \
    "FacadeTests.cs: zbrt transport-override assert leftover"
p.write_text(t, encoding="utf-8", newline="")
drop_brace_block(ft, "public async Task Eval_OverZbrt_FailsClosed(")
drop_brace_block(ft, "public async Task Stream_Zbrt_EventsStop(")
drop_brace_block(ft, "public async Task Stream_Zbrt_RejectsEnvAndPty(")
drop_brace_block(ft, "public async Task Zbrt_Fs_Frames_CarryNullKeysPerProtocol(")
print("  csharp done")
PY

echo "== 3c. 手术：去 mlua（lua 解释器；python 保留）=="
python3 - <<'PY'
import re
from pathlib import Path


def must_replace(path, old, new, count=1):
    p = Path(path)
    t = p.read_text(encoding="utf-8")
    if t.count(old) != count:
        raise SystemExit(f"{path}: pattern {old[:60]!r} count={t.count(old)} expected {count}")
    p.write_text(t.replace(old, new), encoding="utf-8", newline="")


def drop_lines(path, matchers):
    p = Path(path)
    lines = p.read_text(encoding="utf-8").splitlines(keepends=True)
    out, removed = [], 0
    for line in lines:
        if any(m(line) for m in matchers):
            while out and (out[-1].strip().startswith("#[")
                           or out[-1].strip().startswith("///")):
                out.pop()
            removed += 1
            continue
        out.append(line)
    p.write_text("".join(out), encoding="utf-8", newline="")
    return removed


def drop_block(path, start_marker):
    p = Path(path)
    lines = p.read_text(encoding="utf-8").splitlines(keepends=True)
    out, i, removed = [], 0, 0
    while i < len(lines):
        line = lines[i]
        if start_marker in line:
            while out and (out[-1].strip().startswith("#[")
                           or out[-1].strip().startswith("///")):
                out.pop()
            j = i
            while "{" not in lines[j]:
                j += 1
            depth = lines[j].count("{") - lines[j].count("}")
            j += 1
            while j < len(lines) and depth > 0:
                depth += lines[j].count("{") - lines[j].count("}")
                j += 1
            removed += 1
            i = j
            continue
        out.append(line)
        i += 1
    if removed == 0:
        print(f"  [skip] {path}: block {start_marker!r} already absent")
        return
    p.write_text("".join(out), encoding="utf-8", newline="")


# interpreters/mod.rs：lua 模块 + LUA_LIB_DIR。
drop_lines("rfb-runtime/src/interpreters/mod.rs",
           [lambda l: l.strip() == "pub mod lua;"])
drop_lines("rfb-runtime/src/interpreters/mod.rs",
           [lambda l: "LUA_LIB_DIR" in l])

# lib.rs：interpreters 门只留 rustpython。
must_replace("rfb-runtime/src/lib.rs",
             '#[cfg(any(feature = "rustpython", feature = "mlua"))]',
             '#[cfg(feature = "rustpython")]')

# main.rs：lua 分派块。
must_replace("rfb-runtime/src/main.rs", '''    #[cfg(feature = "mlua")]
    if init_script == "lua" {
        std::process::exit(rfb_runtime::interpreters::lua::run(&args[1..]));
    }
''', "")

# rfb-runtime/Cargo.toml：mlua 依赖 + feature。
p = Path("rfb-runtime/Cargo.toml")
t = p.read_text(encoding="utf-8")
new, n = re.subn(r'\nmlua = \{ version = [^\n]*\}', "", t)
if n != 1:
    raise SystemExit(f"rfb-runtime/Cargo.toml: mlua dep match n={n}")
new, n = re.subn(r'\nmlua = \["guest", "dep:mlua"\]', "", new)
if n != 1:
    raise SystemExit(f"rfb-runtime/Cargo.toml: mlua feature match n={n}")
p.write_text(new, encoding="utf-8", newline="")

# commands.rs：--with-lua / --lua-lib-dir 参数（含多行 #[arg] 块）与 mlua
# 默认 features。
for block in (
    '''    /// Install /bin/lua (hardlink to the runtime, `mlua` feature).
    #[arg(
        long,
        help = "Install /bin/lua (hardlink to the runtime; the binary must be built with the mlua feature)"
    )]
    pub with_lua: bool,
''',
    '''    /// Install /bin/lua (requires the pid1 built with the mlua feature).
    #[arg(long)]
    pub with_lua: bool,
''',
):
    must_replace("rfb/src/cli/commands.rs", block, "", count=1)
p = Path("rfb/src/cli/commands.rs")
t = p.read_text(encoding="utf-8")
new, n = re.subn(
    r'    ///[^\n]*\n(?:    ///[^\n]*\n)*    #\[arg\(\n        long,\n        value_name = "DIR",\n        help = "Directory of Lua modules baked into the image \(offline require path\)"\n    \)\]\n    pub lua_lib_dir: Option<PathBuf>,\n',
    "", t)
if n != 1:
    raise SystemExit(f"commands.rs: lua_lib_dir blocks n={n} expected 1")
t = new
p.write_text(t, encoding="utf-8", newline="")
must_replace("rfb/src/cli/commands.rs", 'default_value = "cli,rustpython,mlua"',
             'default_value = "cli,rustpython"')
# lib.rs：firecracker re-export shim 在 minimal 下挂 forkd 门（rfb1 boot
# 驱动的 attach_pdeathsig / read_response 仍依赖它）。
must_replace("rfb/src/lib.rs",
             '#[cfg(all(feature = "zeroboot", target_os = "linux"))]',
             '#[cfg(all(feature = "forkd", target_os = "linux"))]')
must_replace("rfb/Cargo.toml",
             'forkd = ["dep:reqwest", "dep:tokio", "dep:url", "dep:libc"]',
             'forkd = ["dep:reqwest", "dep:tokio", "dep:url", "dep:libc", "rfb-runtime/firecracker"]')

# dispatch.rs：RootfsOptions 构造里的 lua 字段。
drop_lines("rfb/src/cli/dispatch.rs",
           [lambda l: l.strip() in ("with_lua: args.with_lua,",
                                    "lua_lib_dir: args.lua_lib_dir.clone(),",
                                    "lua_lib_dir: args.bake_dirs.lua_lib_dir.clone(),")])

# forkd/backend_up.rs：with_lua 参数链。
drop_lines("rfb/src/cli/forkd/backend_up.rs",
           [lambda l: l.strip() in ("with_lua: bool,", "with_lua,", "args.with_lua,",
                                    "lua_lib_dir: None,")])
# rfb/tests/cli_forkd.rs：ForkdBackendUpArgs 测试字面量里的 with_lua 字段。
drop_lines("rfb/tests/cli_forkd.rs",
           [lambda l: l.strip() in ("with_lua: false,",)])

# image_build/mod.rs：LUA_LIB_DIR 再导出。
must_replace("rfb/src/cli/image_build/mod.rs", "LUA_LIB_DIR, ", "")

# image_build/build.rs：字段/常量/校验块/安装块/manifest。
drop_lines("rfb/src/cli/image_build/build.rs",
           [lambda l: l.strip().startswith("pub with_lua:")
            or l.strip().startswith("pub lua_lib_dir:")
            or "pub const LUA_LIB_DIR" in l
            or "options.lua_lib_dir.as_deref()," in l
            or "install_site_dir(&image_path, options.lua_lib_dir.as_deref(), LUA_LIB_DIR)?;" in l
            or '"lua": options.lua_lib_dir.as_ref().map(|p| p.to_string_lossy()),' in l])
drop_block("rfb/src/cli/image_build/build.rs",
           "if (options.with_lua || options.lua_lib_dir.is_some())")
drop_block("rfb/src/cli/image_build/build.rs", "if options.with_lua {")

# image_build/all.rs：with_lua 派生与构造。
drop_lines("rfb/src/cli/image_build/all.rs",
           [lambda l: l.strip() in ('let with_lua = wants("mlua");', "with_lua,",
                                    "lua_lib_dir: args.lua_lib_dir.clone(),",
                                    "lua_lib_dir: args.bake_dirs.lua_lib_dir.clone(),")])

# image_build/script.rs：build.rfb schema 去 lua 字段（deny_unknown_fields：
# 旧脚本带 lua 键会被拒绝——最小版语义变更，文档同步）。
drop_lines("rfb/src/cli/image_build/script.rs",
           [lambda l: l.strip() in ("pub lua: bool,", "pub lua_lib: Option<PathBuf>,")])
drop_block("rfb/src/cli/image_build/script.rs", "if script.interpreters.lua {")
drop_lines("rfb/src/cli/image_build/script.rs",
           [lambda l: l.strip() in ("with_lua,",
                                    "with_lua: script.interpreters.lua,",
                                    "lua_lib_dir: None,",
                                    "lua_lib_dir: script.packages.lua_lib.as_ref().map(|p| resolve(p)),")])
# zbrt-zbrt 模式整体移除（简版 guest 无 ZBRT 服务；mode 词表收成
# forkd-agent / rfb-vsock）。
must_replace("rfb/src/cli/image_build/script.rs",
             'if !["zeroboot-zbrt", "forkd-agent", "rfb-vsock"].contains(&script.mode.as_str()) {',
             'if !["forkd-agent", "rfb-vsock"].contains(&script.mode.as_str()) {')
must_replace("rfb/src/cli/image_build/script.rs",
             '''"mode must be zeroboot-zbrt, forkd-agent, or rfb-vsock, got {:?}",
            script.mode''',
             '''"mode must be forkd-agent or rfb-vsock, got {:?}",
            script.mode''')
must_replace("rfb/src/cli/image_build/script.rs",
             '//! mode = "zeroboot-zbrt"            # or "forkd-agent" / "rfb-vsock"',
             '//! mode = "forkd-agent"             # or "rfb-vsock"')
must_replace("rfb/src/cli/image_build/script.rs",
             '/// Rootfs mode: `zeroboot-zbrt`, `forkd-agent`, or `rfb-vsock`.',
             '/// Rootfs mode: `forkd-agent` or `rfb-vsock`.')
must_replace("rfb/src/cli/image_build/build.rs",
             '''        "zeroboot-zbrt" => ("/init", "/init", "zbrt"),
        _ => {
            return Err(validation(
                "image mode must be rfb-vsock, forkd-agent, or zeroboot-zbrt",''',
             '''        _ => {
            return Err(validation(
                "image mode must be rfb-vsock or forkd-agent",''')
must_replace("rfb/src/cli/image_build/build.rs",
             '''    let zbrt = mode == "zeroboot-zbrt";
    let floor_mb = if zbrt { 2 } else { 8 };''',
             "    let floor_mb = 8;")
must_replace("rfb/src/cli/image_build/build.rs",
             '''    let headroom_bytes: u64 = if zbrt {
        3 * 1024 * 1024
    } else {
        8 * 1024 * 1024
    };''',
             "    let headroom_bytes: u64 = 8 * 1024 * 1024;")
must_replace("rfb/src/cli/image_build/build.rs",
             '''    let mkfs_args: &[&str] = if zbrt {
        &[
            "-q",
            "-t",
            "ext4",
            "-F",
            "-O",
            "^has_journal",
            "-m",
            "0",
            &image,
        ]
    } else {
        &["-q", "-t", "ext4", "-F", "-m", "0", &image]
    };''',
             '''    let mkfs_args: &[&str] = &["-q", "-t", "ext4", "-F", "-m", "0", &image];''')
drop_block("rfb/src/cli/image_build/build.rs", "if zbrt {")
drop_block("rfb/src/cli/image_build/build.rs", 'if mode == "zeroboot-zbrt" {')
must_replace("rfb/src/cli/commands.rs",
             '''    /// Rootfs mode: `rfb-vsock`, `forkd-agent`, or `zeroboot-zbrt` (default `rfb-vsock`).''',
             '''    /// Rootfs mode: `rfb-vsock` or `forkd-agent` (default `rfb-vsock`).''')
must_replace("rfb/src/cli/commands.rs",
             '''        value_parser = ["rfb-vsock", "forkd-agent", "zeroboot-zbrt"],
        help = "Rootfs mode: rfb-vsock, forkd-agent, or zeroboot-zbrt"''',
             '''        value_parser = ["rfb-vsock", "forkd-agent"],
        help = "Rootfs mode: rfb-vsock or forkd-agent"''', count=2)
must_replace("rfb/src/cli/commands.rs",
             '''    /// Rootfs mode (default `zeroboot-zbrt`).''',
             '''    /// Rootfs mode (default `forkd-agent`).''')
must_replace("rfb/src/cli/commands.rs",
             '        default_value = "zeroboot-zbrt",',
             '        default_value = "forkd-agent",')
must_replace("rfb/src/cli/dispatch.rs",
             '"protocol": ["rfb1", "zbrt"],',
             '"protocol": ["rfb1"],')
must_replace("rfb/tests/cli.rs",
             'assert_eq!(value["protocol"], serde_json::json!(["rfb1", "zbrt"]));',
             'assert_eq!(value["protocol"], serde_json::json!(["rfb1"]));')
# guest_entrypoint.rs / main.rs：去 ZeroBoot 传输分支（feature 已不存在）。
must_replace("rfb-runtime/src/guest_entrypoint.rs",
             '''    /// Serve the ZeroBoot V1 (ZBRT) guest on the ZBRT guest vsock port.
    #[cfg(feature = "zeroboot")]
    ZeroBoot,
''', "")
must_replace("rfb-runtime/src/guest_entrypoint.rs",
             '''    #[cfg(feature = "zeroboot")]
    let zeroboot = matches!(transport, GuestTransport::ZeroBoot);
    #[cfg(not(feature = "zeroboot"))]
    let zeroboot = false;''',
             "    let zeroboot = false;")
must_replace("rfb-runtime/src/guest_entrypoint.rs",
             '''        #[cfg(feature = "zeroboot")]
        GuestTransport::ZeroBoot => crate::zeroboot_guest::run(limits).await,
''', "")
must_replace("rfb-runtime/src/main.rs",
             '''    #[cfg(feature = "zeroboot")]
    let zeroboot_mode = mode.as_deref() == Some("zeroboot")
        || std::env::var("RFB_RUNTIME_MODE").ok().as_deref() == Some("zeroboot")
        // The ZBRT image built by `rfb-cli image build-rootfs --mode
        // zeroboot-zbrt` installs this binary as /init with no argv/env hint;
        // the protocol marker written next to it is the only mode signal.
        || zeroboot_marker_active();
    #[cfg(not(feature = "zeroboot"))]
    let zeroboot_mode = false;''',
             "    let zeroboot_mode = false;")
# guest_entrypoint.rs 测试：zeroboot 用例整块删除（cfg 残留 → unexpected_cfgs）。
p = Path("rfb-runtime/tests/guest_entrypoint.rs")
if p.exists():
    t = p.read_text(encoding="utf-8")
    t = t.replace('''#[cfg(feature = "zeroboot")]
use rfb_runtime::guest_entrypoint::GuestTransport;

''', "")
    new, n = re.subn(
        r'#\[cfg\(feature = "zeroboot"\)\]\n#\[test\]\nfn public_guest_transport_includes_zeroboot_mode\(\)[\s\S]*?\n\}\n',
        "", t)
    if n != 1:
        raise SystemExit(f"guest_entrypoint.rs test: n={n}")
    p.write_text(new, encoding="utf-8", newline="")

# image_build_script.rs：fixture/断言对齐新 schema（lua 字段已删、mode 词表
# 收窄、features_for 恒 cli,rustpython）。
ib = "rfb/tests/image_build_script.rs"
must_replace(ib, '''mode = "zeroboot-zbrt"
output = "out/my.ext4"''', '''mode = "forkd-agent"
output = "out/my.ext4"''')
must_replace(ib, '''[interpreters]
python = true
lua = true
''', '''[interpreters]
python = true
''')
must_replace(ib, '''[packages]
py-site = "sites/py"
lua-lib = "sites/lua"
''', '''[packages]
py-site = "sites/py"
''')
must_replace(ib, '    assert_eq!(script.mode, "zeroboot-zbrt");',
             '    assert_eq!(script.mode, "forkd-agent");')
must_replace(ib, "    assert!(script.interpreters.python && script.interpreters.lua);",
             "    assert!(script.interpreters.python);")
must_replace(ib, '    assert_eq!(features_for(&script), "cli,rustpython,mlua");',
             '    assert_eq!(features_for(&script), "cli,rustpython");')
must_replace(ib, '''    let bad = "schema = \\"nope\\"\\nmode = \\"zeroboot-zbrt\\"\\noutput = \\"a.ext4\\"\\n";''',
             '''    let bad = "schema = \\"nope\\"\\nmode = \\"forkd-agent\\"\\noutput = \\"a.ext4\\"\\n";''')
print("  mlua surgery done")

# ---- Rust SDK 本体：rfb crate 客户端的 ZBRT 传输 ----
drop_lines("rfb/src/client/mod.rs",
           [lambda l: l.strip() == "mod zbrt;"])
must_replace("rfb/src/lib.rs",
             '#[cfg(any(feature = "forkd", feature = "zeroboot"))]\npub mod backend;',
             '#[cfg(feature = "forkd")]\npub mod backend;')
# dispatch.rs runtime_diagnostics：scavenge 只在 zeroboot feature 下存在。
must_replace("rfb/src/cli/dispatch.rs",
             '''    #[cfg(all(feature = "zeroboot", target_os = "linux"))]
    let stale_removed = crate::zeroboot::scavenge_stale_state(None);
    #[cfg(not(all(feature = "zeroboot", target_os = "linux")))]
    let stale_removed = 0;''',
             "    let stale_removed = 0;")
# facade.rs：cfg(zeroboot) 门控的 ZBRT 分支整体删除；cfg_attr 属性行剥掉。
p = Path("rfb/src/client/facade.rs")
t = p.read_text(encoding="utf-8")
t = t.replace('#[cfg_attr(not(feature = "zeroboot"), allow(unused_imports))]\n', "")
t = t.replace('#[cfg_attr(not(feature = "zeroboot"), allow(unused_variables))] ', "")
t = t.replace('#[cfg(feature = "zeroboot")]\nuse super::zbrt;\n', "")
# minimal 只剩 ndjson：serde_json 的 json 不再被用到。
t = t.replace('use serde_json::{json, Value};\n', "use serde_json::Value;\n")
p.write_text(t, encoding="utf-8", newline="")
# readable_file 的用户（zeroboot up 的 kernel 检查）已删，再导出收窄。
must_replace("rfb/src/cli/image_build/mod.rs",
             "pub(crate) use build::{read_zbrt_markers, readable_file, require_readable, resolve_rootfs_source};",
             "pub(crate) use build::{require_readable, resolve_rootfs_source};")


def drop_cfg_zeroboot(path):
    """删除每个 `#[cfg(feature = "zeroboot")]` 属性行及其门控的构造（花括号
    块 / 以 `;` 或 `,` 结尾的单语句），连同其前导 /// 文档行。返回删除数。"""
    p = Path(path)
    lines = p.read_text(encoding="utf-8").splitlines(keepends=True)
    out, i, removed = [], 0, 0
    while i < len(lines):
        line = lines[i]
        if "cfg(feature = \"zeroboot\")" in line and line.strip().startswith("#[cfg("):
            while out and (out[-1].strip().startswith("///")
                           or out[-1].strip().startswith("#[cfg(")):
                out.pop()
            j = i + 1
            while j < len(lines) and lines[j].strip() == "":
                j += 1
            # 单语句（`;`/`,` 结尾）或花括号块
            m = j
            consumed_stmt = False
            while m < len(lines):
                l = lines[m]
                if l.rstrip().endswith(";") or l.rstrip().endswith(","):
                    m += 1
                    consumed_stmt = True
                    break
                if "{" in l:
                    break
                m += 1
            if not consumed_stmt and m < len(lines) and "{" in lines[m]:
                depth = lines[m].count("{") - lines[m].count("}")
                m += 1
                while m < len(lines) and depth > 0:
                    depth += lines[m].count("{") - lines[m].count("}")
                    m += 1
            i = m
            removed += 1
            continue
        out.append(line)
        i += 1
    if removed == 0:
        print(f"  [skip] {path}: no cfg(zeroboot) attrs left")
        return
    p.write_text("".join(out), encoding="utf-8", newline="")


drop_cfg_zeroboot("rfb/src/client/facade.rs")
# types.rs：GuestTransport 枚举去 Zbrt 变体。
must_replace("rfb/src/client/types.rs",
             '''    /// ZBRT v1 binary frames over TCP (requires the `zeroboot` feature).
    #[cfg(feature = "zeroboot")]
    Zbrt,
''', "")
# client_codec.rs 整文件都是 ZBRT 黄金向量 → 删；backend_factory.rs 只是把
# 整文件门从 forkd+zeroboot 收成 forkd（zeroboot 用例在 §3 已删）。
Path("rfb/tests/client_codec.rs").unlink()
must_replace("rfb/tests/backend_factory.rs",
             '#![cfg(all(feature = "forkd", feature = "zeroboot"))]',
             '#![cfg(feature = "forkd")]')
# client_facade.rs：门收成 forkd；ZBRT 假服务器用例整块删除——zbrt fake 的
# 辅助函数（zframe/zout/...）已合并进 tests/common/zbrt.rs（第 2 步删除），
# 这里按名删除全部 zbrt 专属用例（对照当前测试文件逐一枚举，勿凭记忆增删）。
must_replace("rfb/tests/client_facade.rs",
             '#![cfg(all(feature = "forkd", feature = "zeroboot"))]',
             '#![cfg(feature = "forkd")]')
for fn_marker in (
    "async fn facade_zbrt_identical_shapes(",
    "async fn zbrt_error_frame_raises_remote(",
    "async fn zbrt_stream_output_exit_and_cancel(",
    "async fn eval_zbrt_fails_closed_without_sending_frames(",
    "async fn zbrt_hello_first_and_control_connection_reuse(",
    "async fn zbrt_stop_buffers_output_before_cancelack(",
    "async fn zbrt_exec_oversize_stdin_fails_closed_without_connecting(",
    "async fn zbrt_control_connection_reconnects_once_after_stale(",
    "async fn zbrt_handshake_failure_is_transport(",
    "async fn zbrt_argv_over_255_fails_closed_without_connecting(",
    "async fn zbrt_control_connection_read_timeout_is_not_retried(",
):
    drop_block("rfb/tests/client_facade.rs", fn_marker)
p = Path("rfb/tests/client_facade.rs")
t = p.read_text(encoding="utf-8")
# rfb::protocol 只剩 zbrt 用例在用（Kind/Fs/ZBRT_V1_CAPABILITIES）→ 整行删。
t = t.replace("use rfb::protocol::{Fs, Kind, ZBRT_V1_CAPABILITIES};\n", "")
t = re.sub(r'use common::zbrt::\{[^}]*\};\n', "", t)
p.write_text(t, encoding="utf-8", newline="")
# tests/common/mod.rs：minimal 不再编译共享 zbrt fake 与 realvm 助手，
# 连同其 cfg 门（zbrt/realvm 模块行删掉后悬空的 cfg 一并清）。
drop_lines("rfb/tests/common/mod.rs",
           [lambda l: l.strip() in ("pub mod zbrt;", "pub mod realvm;",
                                    '#[cfg(feature = "zeroboot")]',
                                    '#[cfg(all(feature = "zeroboot", target_os = "linux"))]')])

for leftover in ("spawn_zbrt", "FakeZbrt", "GuestTransport::Zbrt", "zerror(", "ZbrtOutput",
                 "ZbrtExit", "ZbrtErrorFrame", "zfsresult", "zhealthack",
                 "zcancelack", "hello_capabilities", "read_exact_blocking",
                 "ZBRT_V1_CAPABILITIES",
                 "zbrt_stop_buffers_output_before_cancelack",
                 "zbrt_exec_oversize_stdin_fails_closed_without_connecting"):
    assert leftover not in t, f"client_facade.rs: {leftover!r} leftover"


def strip_cfg_zeroboot_attrs(path):
    """backend 工厂/入口的 cfg 清理：`#[cfg(feature = "zeroboot")]`（正向）
    连同门控构造一起删；`#[cfg(not(...zeroboot...))]`（反向）只删属性行、
    保留构造（minimal 里这些分支无条件生效/仍 fail-closed）。"""
    p = Path(path)
    lines = p.read_text(encoding="utf-8").splitlines(keepends=True)
    out, i, removed = [], 0, 0
    while i < len(lines):
        line = lines[i]
        if "zeroboot" in line and line.strip().startswith("#[cfg("):
            positive = "not(" not in line
            while out and (out[-1].strip().startswith("///")
                           or (out[-1].strip().startswith("#[cfg(")
                               and "zeroboot" in out[-1])):
                out.pop()
            j = i + 1
            while j < len(lines) and lines[j].strip() == "":
                j += 1
            m = j
            if positive:
                consumed_stmt = False
                while m < len(lines):
                    l = lines[m]
                    if l.rstrip().endswith(";") or l.rstrip().endswith(","):
                        m += 1
                        consumed_stmt = True
                        break
                    if "{" in l:
                        break
                    m += 1
                if not consumed_stmt and m < len(lines) and "{" in lines[m]:
                    depth = lines[m].count("{") - lines[m].count("}")
                    m += 1
                    while m < len(lines) and depth > 0:
                        depth += lines[m].count("{") - lines[m].count("}")
                        m += 1
            i = m
            removed += 1
            continue
        out.append(line)
        i += 1
    if removed == 0:
        print(f"  [skip] {path}: no cfg(zeroboot) attrs left")
        return
    p.write_text("".join(out), encoding="utf-8", newline="")


strip_cfg_zeroboot_attrs("rfb/src/backend.rs")
strip_cfg_zeroboot_attrs("rfb-runtime/src/main.rs")
print("  rust sdk zbrt done")
PY

echo "== 3d. 刷新 Cargo.lock（裁掉的依赖出清，裸产物 lock 自洽）=="
source ~/.cargo/env 2>/dev/null || true
# 编译失败必须可见：日志落盘，失败时把尾部打出来再退出（不能 2>&1 >/dev/null
# 静默吞掉——否则 lock 刷新门形同虚设）。
if ! cargo check --workspace --no-default-features --features forkd,cli \
        > /tmp/minimal-check-forkd-cli.log 2>&1; then
  echo "  cargo check (forkd,cli) FAILED — log tail:"
  tail -25 /tmp/minimal-check-forkd-cli.log
  exit 1
fi
if ! cargo check --workspace --no-default-features \
        > /tmp/minimal-check-nodefault.log 2>&1; then
  echo "  cargo check (no-default-features) FAILED — log tail:"
  tail -25 /tmp/minimal-check-nodefault.log
  exit 1
fi
if grep -q '^name = "mlua"' Cargo.lock; then
  echo "  Cargo.lock 仍含 mlua — 出清失败"
  exit 1
fi
echo "  lock clean"

echo "== 4. 自校验：zeroboot / zbrt / mlua 引用残留（白名单）=="
LEFTOVER=$(grep -rn --include='*.rs' "zeroboot" rfb/src rfb/tests rfb-runtime/tests 2>/dev/null \
  | grep -vE "rfb/src/cli/image_build/(build|artifact)\.rs|rfb/src/image_profiles\.rs|rfb/src/cli/forkd/snapshot\.rs|rfb/src/backend\.rs" \
  | grep -vE "rfb/tests/(backend_factory|cli_image_build|cli|artifact_sidecar_contract)\.rs" \
  | grep -vE "zeroboot_protocol|rfb-zeroboot|ZBRT|crate::zeroboot::|//|zeroboot/firecracker" \
  || true)
# 白名单说明：image_build/{build,artifact}.rs、image_profiles.rs、
# forkd/snapshot.rs、backend.rs 保留的是【制品命名/识别层】——简版不提供
# zeroboot 运行时，但仍要能识别外部产出的 zbrt 制品格式（provenance 验证）；
# backend_factory/cli_image_build 的命中是 fail-closed 文案与 profile 映射测试。
if [ -n "$LEFTOVER" ]; then
  echo "  rust zeroboot 残留（需要人工裁决或补充清单）："
  echo "$LEFTOVER" | head -15
  exit 1
fi
echo "  rust clean"

LUALO=$(grep -rn --include='*.rs' -iE "mlua|with_lua|lua_lib|LUA_LIB" rfb/src rfb-runtime/src 2>/dev/null \
  | grep -vE "//|evaluat" || true)
if [ -n "$LUALO" ]; then
  echo "  rust lua 残留（需要人工裁决或补充清单）："
  echo "$LUALO" | head -15
  exit 1
fi
echo "  rust lua clean"

SDKLO=$(grep -rn -iE "zbrt|zeroboot" sdk/python/rfb_sdk sdk/nodejs/src \
  sdk/java/src sdk/csharp/src 2>/dev/null | grep -vE "//|/\*|\* " || true)
if [ -n "$SDKLO" ]; then
  echo "  sdk zbrt 残留（需要人工裁决或补充清单）："
  echo "$SDKLO" | head -15
  exit 1
fi
echo "  sdk clean"

if [ "$CHECK" = "1" ]; then
  echo "== 5. 简版编译门禁 =="
  source ~/.cargo/env 2>/dev/null || true
  # 编译门（含 --all-targets：测试树编译坏必须在这里炸，不能漏）。
  cargo check --workspace --locked --all-targets --no-default-features --features forkd,cli 2>&1 | tail -2
  cargo check --workspace --locked --all-targets --no-default-features 2>&1 | tail -1
  # 测试门：日志落盘后检查失败标记——测试编译失败时没有 `test result` 行，
  # 靠 grep 结果行的旧写法会把编译失败吞成绿灯（|| true 兜底的教训）。
  cargo test --workspace --features forkd,cli > /tmp/minimal-test.log 2>&1 || {
    grep -E "^error|could not compile|FAILED|panicked" /tmp/minimal-test.log | head -10
    exit 1
  }
  grep -E "^test result" /tmp/minimal-test.log | grep -v " 0 failed" | head -5 || true
  echo "== 6. SDK 测试门 =="
  (cd sdk/python && { python3 -m pytest tests -q 2>/dev/null || python3 -m unittest discover -s tests -t . 2>&1 | tail -4; })
  (cd sdk/nodejs && npm install --no-audit --no-fund >/dev/null 2>&1 && npm test 2>&1 | tail -3)
  (cd sdk/java && mvn -q install -DskipTests >/dev/null 2>&1 && cd tests && mvn -q test 2>&1 | tail -3)
  # C# 门：无 dotnet 环境时明确提示（不静默跳过）。
  if command -v dotnet >/dev/null 2>&1; then
    (cd sdk/csharp && dotnet build -v q 2>&1 | tail -2 && dotnet test --no-build -v q 2>&1 | tail -3)
  else
    echo "WARN: dotnet not found — csharp gate SKIPPED (manual: dotnet test sdk/csharp)"
  fi
  echo "SIMPLE_OK"
fi
echo "SPLIT_DONE → $TARGET"
