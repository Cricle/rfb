# RFB

RFB（Runtime/Resource/Filesystem Boundary）是面向沙箱的 Rust workspace：定义宿主与 guest 之间的类型化契约、传输和运行时集成。它不是聊天服务、模型代理或应用会话层。

## crates.io 安装

发布后可直接添加（当前 workspace 版本为 `0.1.0`）：

```toml
[dependencies]
rfb = { version = "0.1", features = ["forkd"] }
rfb-rig = "0.1"
# 只有需要 guest/Firecracker 运行时才添加：
rfb-runtime = { version = "0.1", default-features = false, features = ["host-vsock"] }
```

也可以使用 CLI：`cargo install rfb --features cli`。crates.io 包只包含 Rust 源码与 crate 资源，不包含 Firecracker、Linux kernel、ext4 镜像、快照或 forkd 服务；这些必须由部署系统显式提供。

## crate 职责

| crate | 职责 |
|---|---|
| `rfb` | 最小沙箱 API、能力/文件系统/guest 协议抽象；可选 forkd、ZBRT 与 `rfb-cli` 集成。|
| `rfb-rig` | 将 RFB 沙箱能力适配为 Rig 0.42 工具；不拥有 Agent loop。|
| `rfb-runtime` | RFB1 guest/runtime、workspace executor、vsock、Firecracker 控制器和运行时测试；也提供构建/验收所需库代码。|

依赖方向为 `rfb-rig`/`rfb-runtime` → `rfb`；`rfb` 不依赖 Rig。应用负责 Agent、提示词、模型、HTTP/SSE、凭据和会话存储。

## features、平台与 MSRV

默认情况下 `rfb` 不启用后端；`forkd` 启用 TCP/NDJSON forkd 客户端，`zeroboot` 启用 ZBRT，`cli` 启用全部后端及 `rfb-cli`。`rfb-runtime` 默认启用 `core`：`guest` 为 guest/vsock，`host-vsock` 为宿主 vsock，`forkd` 为 guest forkd agent，`firecracker` 为 Linux/KVM 控制器，`cli` 启用 CLI 所需全部功能。

Linux 才提供 `tokio-vsock`、KVM ioctl 与 Firecracker 控制器；Windows 可编译 core/CLI（不提供 Linux KVM/vsock），macOS 可编译不依赖 Linux 后端的部分。真实 VM 验收需要 Linux/WSL、KVM、匹配的 Firecracker、kernel 和 rootfs。MSRV 为 Rust 1.82（edition 2021）；发布与 CI 应使用该版本或更新版本验证。覆盖率使用轻量的 `cargo-tarpaulin`，不使用 LLVM、LLM 或常驻外部服务；nightly/manual workflow 对每个 crate 执行 90% line 门禁。

## 快速开始

```bash
cargo add rfb
cargo test -p rfb
cargo test -p rfb-runtime --no-default-features --features core
cargo build -p rfb --features cli
```

`rfb-cli doctor --json` 可检查宿主能力；`image build-rootfs`、`rfb1 acceptance` 等命令的路径和运行时输入必须显式传入。协议 RFB1（framed vsock）、forkd（TCP/NDJSON）和 ZBRT（ZeroBoot binary frame）及其镜像不可互换，缺少能力时必须 fail closed。

## 大型运行时资产边界

Git/crates.io 源码边界内保留协议、构建代码、镜像 manifest 示例和文档；不把大型或平台相关资产当作 crate 依赖发布：Firecracker 二进制、Linux kernel、ext4 rootfs、快照、forkd agent 镜像、运行时静态二进制、日志和构建目录均是外部制品。它们应由受控制品仓库/部署流水线按目标平台下载、校验 SHA-256，并通过绝对路径或环境变量注入（例如 `RFB_RUNTIME_BIN`、`FORKD_ROOTFS`）。本仓库保留 `firecracker-v1.16.1-x86_64.tgz` 作为唯一完整 Firecracker 发布资产，校验方式为 `sha256sum -c firecracker-v1.16.1-x86_64.tgz.sha256`；压缩包内的 LICENSE、NOTICE、THIRD-PARTY 必须随再分发保留。不要把凭据、真实快照或用户 workspace 内容提交到仓库。

## 发布

发布顺序是：先发布 `rfb-runtime`，再发布 `rfb`，最后发布依赖两者的 `rfb-rig`。每个版本先跑 format、测试、文档和 feature matrix，再执行 `cargo package --locked` 与 `cargo publish --dry-run --locked` 检查包内容；正式发布只允许从干净的 `vMAJOR.MINOR.PATCH` tag workflow 执行。当前 workspace metadata 使用 `PLACEHOLDER_ORG` 占位地址，启用 GitHub Release 前必须替换为真实仓库。

更多镜像、协议、验收与运维约束见 `requirements/RFB/0.1.0/`。
