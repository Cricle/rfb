---
name: rfb-image-build
version: 1.0.0
description: 构建包含指定解释器（python/lua）和用户选择包的沙箱镜像。当 AI 需要创建包含特定运行时和依赖包的 forkd 沙箱镜像时使用。
---

# 沙箱镜像构建（含解释器 + 用户包）

## 前置

- 宿主机 Linux/WSL，已安装 Rust toolchain + musl target
- `rfb-cli doctor` 全部通过

## 场景 A：构建包含 Python + 自选包的镜像

```bash
rfb-cli image build-all \
  --root . \
  --target x86_64-unknown-linux-musl \
  --package rfb-runtime \
  --interpreter python \
  --packages numpy requests
```

### 用户选包流程

1. 询问用户需要哪些 pip 包（逗号分隔）
2. 将包列表写入 rootfs 的 pip 预装清单（或通过 build-all 参数传递）
3. build-all 会：musl 编译 → rootfs 组装（含 python 解释器 + pip 包离线注入）→ 内核校验 → Firecracker verify
4. 产物：`resx/rootfs/forkd-agent.ext4`

### 限制

- pip 包需纯 Python 或有 musl 兼容的 C 扩展（嵌入 musl 无 glibc）
- 包大小受 rootfs 容量限制

## 场景 B：构建包含 Lua 的镜像

```bash
rfb-cli image build-all \
  --root . \
  --target x86_64-unknown-linux-musl \
  --package rfb-runtime \
  --interpreter lua
```

Lua 5.4 静态编译进 agent，无额外包管理器。通过 guest `eval` 执行 Lua 代码。

## 场景 C：同时包含 Python 和 Lua

```bash
rfb-cli image build-all \
  --root . \
  --target x86_64-unknown-linux-musl \
  --package rfb-runtime \
  --interpreter python \
  --interpreter lua
```

## 后续操作

镜像构建完成后，创建快照并启动沙箱：

```bash
rfb-cli forkd snapshot-create --tag <snap-tag> --tap forkd-tap0
# 等待 ready + bootable
rfb-cli forkd sandbox-create --tag <snap-tag>
```
