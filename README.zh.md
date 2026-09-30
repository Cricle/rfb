# RFB

[![Release](https://github.com/Cricle/rfb/actions/workflows/release.yml/badge.svg)](https://github.com/Cricle/rfb/actions/workflows/release.yml)
[![Real-VM E2E](https://github.com/Cricle/rfb/actions/workflows/e2e.yml/badge.svg)](https://github.com/Cricle/rfb/actions/workflows/e2e.yml)
[![Crates.io](https://img.shields.io/crates/v/rfb-sdk)](https://crates.io/crates/rfb-sdk)
[![PyPI](https://img.shields.io/pypi/v/rfb-sdk)](https://pypi.org/project/rfb-sdk/)
[![NuGet](https://img.shields.io/nuget/v/Rfb.Sdk)](https://www.nuget.org/packages/Rfb.Sdk/)
[![Maven Central](https://img.shields.io/maven-central/v/io.github.cricle/rfb-sdk)](https://central.sonatype.com/artifact/io.github.cricle/rfb-sdk)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)

[English](README.md) | **简体中文**

RFB（Runtime/Resource/Filesystem Boundary）是面向沙箱的 Rust workspace：定义宿主与 guest 虚拟机之间的类型化契约、传输和运行时集成。它不是聊天服务、模型代理或应用会话层。

## 安装（Rust）

```toml
[dependencies]
rfb-sdk = "0.0.1"
rfb-rig = "0.0.1"
# 只有需要 guest/Firecracker 运行时才需要：
rfb-runtime = { version = "0.0.1", default-features = false, features = ["host-vsock"] }
```

CLI：`cargo install rfb-sdk --features cli`（二进制名 `rfb-cli`）。预编译的 **linux-x64** `rfb-cli` 二进制也以纯二进制包分发（不含任何包装壳）：

```bash
dotnet add package Rfb.Cli   # NuGet（二进制位于 tools/，复制到输出目录）
npm install -g rfb-cli       # npm（bin 入口直连 ELF）
```

### 用 `build.rfb` 脚本自定义镜像

一个 TOML 文件声明整张镜像——解释器、离线包、预编译 Rust 应用、附加文件——`rfb-cli image build-script` 一条命令跑完整条链（静态 runtime 构建 → rootfs 组装 → 校验）：

```toml
schema = "rfb-build/v1"
mode = "zeroboot-zbrt"
output = "out/my-sandbox.ext4"
force = true

[interpreters]
python = true        # /bin/python3（内嵌 RustPython）
lua = true           # /bin/lua（内嵌 mlua 5.4）

[packages]
py-site = "sites/py" # 离线纯 Python 包 → /usr/lib/python3/site-packages
lua-lib = "sites/lua"

[rust]
apps = ["hello"]     # 预编译 musl 二进制 → /usr/local/bin

[files]
"assets/banner.txt" = "/etc/motd"
```

带注释的参考脚本见 [docs/example-build.rfb](docs/example-build.rfb)。解释器 fail-closed：runtime 没编 `rustpython`/`mlua` feature 时镜像构建直接报错，绝不装死链接。

crates.io 包只包含 Rust 源码与 crate 资源，不包含 Firecracker、Linux kernel、ext4 镜像、快照或 forkd 服务；这些必须由部署系统显式提供。

## crate 职责

| crate | 职责 |
|---|---|
| `rfb-sdk` | 最小沙箱 API、能力/文件系统/guest 协议抽象；可选 forkd、ZBRT 与 `rfb-cli` 集成。|
| `rfb-rig` | 将 RFB 沙箱能力适配为 Rig 0.42 工具；不拥有 Agent loop。|
| `rfb-runtime` | RFB1 guest/runtime、workspace executor、vsock、Firecracker 控制器和运行时测试；也提供构建/验收所需库代码。|

依赖方向为 `rfb-rig`/`rfb-runtime` → `rfb-sdk`；`rfb-sdk` 不依赖 Rig。应用负责 Agent、提示词、模型、HTTP/SSE、凭据和会话存储。

## 多语言 SDK

同一套沙箱能力还提供：

| 语言 | 包 |
|---|---|
| Python | [`rfb-sdk`](https://pypi.org/project/rfb-sdk/)（`import rfb_sdk`） |
| Java | [`io.github.cricle:rfb-sdk`](https://central.sonatype.com/artifact/io.github.cricle/rfb-sdk)（Maven Central） |
| C#/.NET | [`Rfb.Sdk`](https://www.nuget.org/packages/Rfb.Sdk/)（NuGet） |
| Node.js | [`rfb-sdk`](https://www.npmjs.com/package/rfb-sdk)（`import { RfbClient } from 'rfb-sdk'`） |

四个套件（Python / C# / Java / Node.js）都在 CI 的干净 Debian 12 容器里做端到端验证（见 `sdk/e2e/debian-e2e.sh`）。

## features、平台与 MSRV

默认 `rfb-sdk` 不启用任何后端：`forkd` 启用 TCP/NDJSON forkd 客户端，`zeroboot` 启用 ZBRT，`cli` 启用全部后端及 `rfb-cli`。`rfb-runtime` 默认启用 `core`：`guest` 为 guest/vsock，`host-vsock` 为宿主 vsock，`forkd` 为 guest forkd agent，`firecracker` 为 Linux/KVM 控制器，`cli` 启用 CLI 所需全部功能。

`tokio-vsock`、KVM ioctl 与 Firecracker 控制器仅 Linux 提供；Windows 可编译 core/CLI（无 Linux KVM/vsock），macOS 可编译不依赖 Linux 后端的部分。真实 VM 验收需要 Linux/WSL、KVM、匹配的 Firecracker、kernel 和 rootfs。MSRV 为 Rust 1.93（edition 2021），由 Cargo.lock 依赖下限决定。

## 快速开始

```bash
cargo add rfb-sdk
cargo test -p rfb-sdk
cargo test -p rfb-runtime --no-default-features --features core
cargo build -p rfb-sdk --features cli
rfb-cli doctor --json   # 宿主能力报告
```

协议线——RFB1（framed vsock）、forkd（TCP/NDJSON）、ZBRT（ZeroBoot binary frame）——互不通用；缺少能力时必须 fail closed。

## 热启动、100 并发与集群沙箱

默认值即实测最佳配置：`RFB_ZBRT_VM_MEM_MIB=128`（Firecracker 实测下限 48）、`RFB_ZBRT_VM_VCPU=1`、`RFB_ZBRT_SESSIONS=8`（单 VM 并发命令数；实测 16 会话有效并发 14.97）、`RFB_ZBRT_SNAPSHOT_SHARDS=2`（热启动下每分片独立父快照与锁，恢复完全并行——实测 100 并发 create **100/100**，2 分片摊薄 ~106ms/个、4 分片 ~54ms；CI 门禁 `tests/zeroboot_concurrency.rs`）。快照文件自动收紧为 0700/0600（`memory.bin` 含 VM 全量内存）。2GB 内存机器：热启动 + 48MiB/VM，100 个活 VM 峰值 **PSS 仅 ~170MiB**（RSS 会把共享页重复计入 8 倍以上，勿按 RSS 判断容量）。

`rfb::cluster::ClusterProvider` 把 N 台 forkd controller 节点组成集群：按最少在途（或轮询）调度沙箱创建；传输失败、5xx、响应不可解析自动 failover，连续 3 次失败熔断，之后默认每 30s 惰性半开自动试探一次（`recovery_interval`，试探 create 成功即恢复），`probe()` 可手动立即巡检（不计失败）。4xx 直接向调用方报错、不计数不转移（避免双创建）；传输/解析失败后对账节点沙箱列表、仅删除窗口内唯一孤儿（宁漏不误）。create 返回的 handle 用 RAII 释放在途计数（drop 即释放，无需手工 release）；`preflight()` 校验各节点快照 ready/bootable 且 digest 一致，`list_all()` + `delete_on()` 用于属主宕机后的孤儿清理。exec/stream/文件直连 guest 地址，销毁路由回所属节点。见 `sdk/BACKEND_SETUP.md` §7 与 `rfb/tests/cluster.rs`。

**沙箱 fork（zeroboot）**：`ZeroBootSandbox::fork(self)` checkpoint 活动 VM（pause → 全量快照）后恢复出两个全新 VM，返回 `(原沙箱延续, fork)`——workspace tmpfs 属 guest 内存、被 dump 整体捕获，fork 继承 fork 时刻的全部状态（真机验证：fork 前写入的文件在 fork 与 fork 的 fork 中均可读）。单次 fork ~0.5-1s（补丁版无增量 diff，forkd 的 live-fork 才有）；checkpoint 目录由两个子沙箱共享持有、最后一个 drop 自动删除；仅热模式（`RFB_ZBRT_SNAPSHOT_DIR`）可 fork。见 `sdk/BACKEND_SETUP.md` §6b-4 与 `rfb/tests/zeroboot_fork.rs`。

## 大型运行时资产边界

协议代码、构建代码、镜像 manifest 示例和文档在 git/crates.io 边界内；大型/平台相关资产刻意不作为 crate 依赖发布：Firecracker 二进制、Linux kernel、ext4 rootfs、快照、forkd agent 镜像、运行时静态二进制、日志和构建目录均为外部制品，由受控制品仓库/部署流水线按目标平台下载、校验 SHA-256，并通过绝对路径或环境变量注入（例如 `RFB_RUNTIME_BIN`、`FORKD_ROOTFS`）。

本仓库保留 `resx/firecracker/firecracker-v1.16.1-x86_64.tgz` 作为唯一完整 Firecracker 发布资产，校验方式为 `sha256sum -c resx/firecracker/firecracker-v1.16.1-x86_64.tgz.sha256`；压缩包内的 LICENSE、NOTICE、THIRD-PARTY 必须随再分发保留。不要把凭据、真实快照或用户 workspace 内容提交到仓库。

## CI/CD

- **CI**——所有 PR 与非 main 分支推送：Rust 门禁（fmt、clippy、测试、文档，全部 `--all-features`，含边界检查）+ 干净 Debian 12 容器里的 SDK 全量构建与测试（Python / C# / Java / Node.js）。
- **真机 E2E**——push 到 `main`：在 KVM runner 上启动真实 Firecracker/forkd 栈，从 forkd-agent rootfs 创建快照，跑全部 `#[ignore]` 门控的真机测试。
- **Release**——推送 `v主.次.补丁` tag：按 `rfb-runtime` → `rfb-sdk` → `rfb-rig` 顺序发布到 crates.io，`rfb-sdk` 发布到 PyPI、`io.github.cricle:rfb-sdk` 发布到 Maven Central（GPG 签名）、`Rfb.Sdk` 发布到 NuGet、`rfb-sdk`（TypeScript）与 `rfb-cli`（仅二进制）发布到 npm，并把 `rfb-cli` 二进制与 SHA256SUMS 附件挂到 GitHub Release。发布幂等：重跑时已发布的产物自动跳过。

## 发布与 CI

完整发布流水线、一次性配置与失败速查见 [docs/RELEASE.md](docs/RELEASE.md)。

## 许可证

MIT — 见 [LICENSE-MIT](LICENSE-MIT)。
