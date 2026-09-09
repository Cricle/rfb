# rfb-runtime

`rfb-runtime` 是 RFB 的 guest/runtime crate，提供 RFB1 framed-vsock 服务、workspace executor、会话与取消、资源/策略、forkd agent，以及 Linux Firecracker 控制器。它只执行沙箱协议，不负责模型调用、Agent loop、用户认证或应用会话。

## 安装与 feature

```toml
[dependencies]
rfb-runtime = { version = "0.1", default-features = false, features = ["core"] }
```

`core`（也是默认 feature）提供异步核心；`guest` 加入 guest 进程、JSON 配置和 vsock；`host-vsock` 提供宿主侧 vsock 客户端；`forkd` 基于 `guest` 提供 forkd agent；`firecracker` 提供 Linux/KVM 控制器；`cli` 启用 guest、forkd、firecracker 及运行时二进制。按需关闭默认 feature，避免把平台后端带入普通库消费者。

MSRV 为 Rust 1.82，edition 2021。`core` 可在常见 Rust 平台编译；vsock 与 Firecracker/KVM 后端仅支持 Linux（Windows/macOS 不提供这些后端）。真实 RFB1 验收还需要 Linux/WSL、KVM、匹配版本的 Firecracker、kernel 和可写 rootfs；缺少它们时普通编译/单元测试仍可运行，但严格验收必须失败而不是伪报 PASS。

## 三个独立协议

- **RFB1**：Firecracker vsock 上的 framed protocol，运行时环境由 `/etc/rfb-runtime/environment` 声明，默认 workspace 为 `/workspace`。
- **forkd**：controller REST 加 guest TCP/NDJSON；使用 forkd-agent 镜像。
- **ZBRT**：独立的 ZeroBoot binary frame（实现和入口在 `rfb`）。

协议、入口、kernel、rootfs 与 snapshot 必须匹配，不能互换。guest 文件系统操作限定在 `/workspace`；能力未声明或不支持时 fail closed。取消会终止子进程并只发送一个 `turn.cancelled` 终态。

## CLI 验证

CLI 位于 `rfb`：

```bash
cargo build -p rfb --features cli
rfb-cli doctor --json
rfb-cli image build-static --target x86_64-unknown-linux-musl --package rfb-runtime
rfb-cli image build-rootfs /abs/path/to/rfb-runtime --mode forkd-agent /abs/out/rootfs.ext4
rfb-cli rfb1 acceptance --require-vm --kernel /abs/path/to/vmlinux --rootfs /abs/path/to/rootfs.ext4
```

所有 runtime、kernel、rootfs、Firecracker、forkd URL/snapshot 输入都必须显式提供，不推断开发机路径。`--require-vm` 在能力缺失时以非零状态退出。

## 运行时资产边界

crate 包和源码不携带 Firecracker 可执行文件、Linux kernel、ext4 rootfs、快照、forkd-agent 镜像或静态 runtime 二进制。这些大型、平台相关且可变的资产由镜像/部署流水线单独构建或下载，必须校验来源与 SHA-256，再以绝对路径注入。不要将它们复制进 crates.io 包，也不要提交凭据、运行日志、用户 workspace 或构建目录。

## 发布顺序

先发布 `rfb`，待 crates.io 可解析后再发布 `rfb-rig` 与 `rfb-runtime`；发布前用 `cargo test --all-features`（Linux 后端按平台执行）、`cargo doc` 和 `cargo package` 检查 feature/包边界。运行时镜像、kernel、snapshot 与 Firecracker 不随 crate 发布，应作为带目标三元组和 SHA-256 的独立制品发布。

详细构建、验收、安全门禁见仓库根 README 与 `requirements/RFB/0.1.0/`。
