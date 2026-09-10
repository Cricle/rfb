---
name: rfb-build-all
version: 1.0.0
description: rfb 沙箱镜像一键构建与真机验收 runbook（本文件内嵌于 rfb-cli，随版本同步）
---

# rfb 沙箱镜像 build-all 一键构建与验收

驱动 rfb-cli 完成：静态编译 → rootfs 组装（含解释器/离线扩展包注入）→ 内核校验 → Firecracker 真机验证 → 解释器 E2E → 全门禁。全程禁止落临时脚本，只用直接命令。

## 关键环境事实（必须遵守）

- 宿主 Windows 时经 WSL（发行版 `Debian`）驱动；WSL 非登录 shell 没有 cargo PATH：每条命令以 `source \$HOME/.cargo/env && ` 开头。
- musl 静态编译必须设：`CC_x86_64_UNKNOWN_LINUX_MUSL=musl-gcc CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc`。
- 产物目标目录：`CARGO_TARGET_DIR=/home/huaji/rfb-target`。
- **改了 rfb-runtime/rfb 源码后必须先重建 rfb-cli debug 二进制再用它跑 build-all**，否则跑的是旧逻辑。
- `python3`/`lua` 裸调用 = 从 stdin 读脚本（REPL 语义），空 stdin → exit 0；usage exit 2 只由多余参数触发。
- Firecracker 门禁 v1.12.x：用 `resx/firecracker/firecracker-v1.12.1`。
- 内核固定用 `resx/kernel/vmlinux-arcbox-0.0.24`（arcboxlabs v0.0.24）。

## 流程

### 1. 重建 rfb-cli（源码有改动时）

```bash
wsl -d Debian -- bash -c "source \$HOME/.cargo/env && cd /mnt/c/workplace/mm/monitor/rfb && CARGO_TARGET_DIR=/home/huaji/rfb-target cargo build -p rfb-sdk --features cli 2>&1 | tail -3 && echo CLI_REBUILT"
```

### 2. build-all（后台跑，musl release + lto 需 1-7 分钟）

```bash
wsl -d Debian -- bash -c "source \$HOME/.cargo/env && cd /mnt/c/workplace/mm/monitor/rfb && CC_x86_64_UNKNOWN_LINUX_MUSL=musl-gcc CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc CARGO_TARGET_DIR=/home/huaji/rfb-target /home/huaji/rfb-target/debug/rfb-cli image build-all --root /mnt/c/workplace/mm/monitor/rfb --features cli,rustpython,mlua /tmp/rfb-build/rootfs.ext4 --mode zeroboot-zbrt --force --py-site-dir /home/huaji/rfb-sites/py --lua-lib-dir /home/huaji/rfb-sites/lua --kernel resx/kernel/vmlinux-arcbox-0.0.24 --firecracker resx/firecracker/firecracker-v1.12.1 2>&1 | tail -40"
```

- `--features` 决定解释器：含 `rustpython` → 打包 `/bin/python3`（RustPython），含 `mlua` → `/bin/lua`；只带 `cli` 则纯 shell（去掉对应 `--py-site-dir`/`--lua-lib-dir`）。
- 离线扩展包：`--py-site-dir` 注入 `/usr/lib/python3/site-packages`，`--lua-lib-dir` 注入 `/usr/lib/lua/5.4`；文件必须是普通文件（拒绝 symlink），逐文件 digest 校验。
- 成功标志：`/tmp/rfb-build/rootfs.ext4` + `.artifact.json` + `.manifest.json` + `.sha256`；manifest 含 `interpreters` 与 `site_dirs` 字段。
- 失败排查：`unrecognized subcommand 'build-all'` = rfb-cli 未重建；`File not found by ext2_lookup` = 镜像内祖先目录缺失（install_site_dir 已修复，勿回退）；链接报 `errno/_dl_x86_cpu_features` 未定义 = musl CC/LINKER env 没设。

### 3. ZBRT 协议 verify（~10s）

```bash
wsl -d Debian -- bash -c "cd /mnt/c/workplace/mm/monitor/rfb && /home/huaji/rfb-target/debug/rfb-cli zeroboot verify --kernel resx/kernel/vmlinux-arcbox-0.0.24 --rootfs /tmp/rfb-build/rootfs.ext4 --firecracker resx/firecracker/firecracker-v1.12.1 --json 2>&1 | tail -15"
```

断言 JSON `"status": "passed"`（echo/true/false/concurrent/malformed 全套）。

### 4. 真机解释器 E2E（三重门控：linux+cli cfg / #[ignore] / RFB_REAL_E2E=1）

```bash
wsl -d Debian -- bash -c "source \$HOME/.cargo/env && cd /mnt/c/workplace/mm/monitor/rfb && CARGO_TARGET_DIR=/home/huaji/rfb-target RFB_REAL_E2E=1 RFB_E2E_ROOTFS_DIR=/tmp/rfb-build cargo test -p rfb-sdk --features cli --test zeroboot_interpreters -- --ignored --test-threads=1 2>&1 | tail -8"
```

覆盖：python3 -c / stdin / site 包 import / lua require / 语法错 exit 1 / 缺文件 exit 1 / usage exit 2 / deadline kill。features 不含 rustpython 或 mlua 时镜像内无对应解释器，跳过并说明。

### 5. 门禁（可并行）

Windows 侧（Git Bash 直接跑）：

```bash
cargo fmt --all -- --check
bash scripts/check-tests-folder.sh
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

WSL 侧（仅当 rfb-runtime 解释器相关代码有改动）：

```bash
wsl -d Debian -- bash -c "source \$HOME/.cargo/env && cd /mnt/c/workplace/mm/monitor/rfb && CC_x86_64_UNKNOWN_LINUX_MUSL=musl-gcc CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc CARGO_TARGET_DIR=/home/huaji/rfb-target cargo clippy -p rfb-runtime --features rustpython,mlua --all-targets --target x86_64-unknown-linux-musl -- -D warnings 2>&1 | tail -3"
```

## 输出要求

最后汇报：build-all 产物路径与大小、manifest interpreters/site_dirs、verify 与 E2E 结果、门禁清单。任何一步失败就地报告原因，不要跳过继续。
