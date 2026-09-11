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

`tokio-vsock`、KVM ioctl 与 Firecracker 控制器仅 Linux 提供；Windows 可编译 core/CLI（无 Linux KVM/vsock），macOS 可编译不依赖 Linux 后端的部分。真实 VM 验收需要 Linux/WSL、KVM、匹配的 Firecracker、kernel 和 rootfs。MSRV 为 Rust 1.90（edition 2021），由 Cargo.lock 依赖下限决定。

## 快速开始

```bash
cargo add rfb-sdk
cargo test -p rfb-sdk
cargo test -p rfb-runtime --no-default-features --features core
cargo build -p rfb-sdk --features cli
rfb-cli doctor --json   # 宿主能力报告
```

协议线——RFB1（framed vsock）、forkd（TCP/NDJSON）、ZBRT（ZeroBoot binary frame）——互不通用；缺少能力时必须 fail closed。

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
