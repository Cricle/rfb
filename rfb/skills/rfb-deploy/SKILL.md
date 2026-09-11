---
name: rfb-deploy
version: 1.0.0
description: 部署 RFB 沙箱环境到 WSL 或真实 Linux：安装依赖 → 启动 controller → 创建快照 → 验证沙箱 → 运行 SDK 测试。当 AI 需要搭建完整开发/测试环境或验证部署时使用。
---

# RFB 沙箱环境部署（WSL / 真实 Linux）

## 前置条件

| 组件 | WSL | 真实 Linux |
|---|---|---|
| Rust toolchain | ✅ | ✅ |
| musl target | `rustup target add x86_64-unknown-linux-musl` | 同左 |
| Firecracker | ✅（WSL2 内核支持 KVM） | ✅（需 /dev/kvm） |
| tap 设备 | ✅ | ✅ |
| e2fsprogs (mke2fs/debugfs) | ✅ | ✅ |

## 步骤

### 1. 环境预检

```bash
rfb-cli doctor
# 确认所有必需工具就绪
```

### 2. 构建 agent rootfs

```bash
rfb-cli image build-rootfs \
  --root . \
  --target x86_64-unknown-linux-musl \
  --package rfb-runtime \
  --mode forkd-agent \
  resx/rootfs/forkd-agent.ext4
```

### 3. 启动 forkd controller

```bash
forkd-controller serve \
  --bind 127.0.0.1:8889 \
  --state /tmp/forkd-state.json \
  --audit-log /tmp/forkd-audit.log \
  --snapshot-root ~/.local/share/forkd/snapshots &
```

### 4. 创建 TAP 设备

```bash
sudo ip tuntap add dev forkd-tap0 mode tap
sudo ip addr add 10.42.0.1/24 dev forkd-tap0
sudo ip link set forkd-tap0 up
```

### 5. 创建快照

```bash
export FORKD_ROOTFS=resx/rootfs/forkd-agent.ext4
export FORKD_KERNEL=resx/kernel/vmlinux-arcbox-0.0.24
rfb-cli forkd snapshot-create --tag my-snap --tap forkd-tap0
```

### 6. 等待快照就绪

```bash
# daemon 异步收编，约 300s
# 轮询直到 snapshot-info 显示 status=ready && bootable=true
rfb-cli forkd snapshot-info --tag my-snap
```

### 7. 创建沙箱并验证

```bash
rfb-cli forkd sandbox-create --tag my-snap
# SDK 连接：
#   RfbClient::from_env() → create_sandbox("my-snap") → exec/eval/文件操作
```

### 8. 验收

```bash
rfb-cli forkd acceptance --tag my-snap --require-vm
```

## 常见问题

| 问题 | 原因 | 处置 |
|---|---|---|
| `/dev/kvm` 不存在 | WSL2 未启用嵌套虚拟化或裸机无 KVM | WSL2: 确认 `.wslconfig` 启用嵌套虚拟化 |
| 快照 300s 未 ready | daemon 异步扫描有周期 | 等待 900s（CI 已设超时） |
| tap 设备创建失败 | 无 CAP_NET_ADMIN | 用 root/sudo |
| Firecracker 版本不匹配 | 快照与 Firecracker 版本绑定 | 使用 resx 内置的 v1.16.1 |
