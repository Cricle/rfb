---
name: rfb-cli-usage
version: 2.0.0
description: 教 AI 使用 rfb-cli 与 forkd 控制器/沙箱交互：创建快照 → 创建沙箱 → exec/读写文件 → 删除。覆盖宿主预检、NDJSON/ZBRT 双传输选择、常见错误与处置。当 AI 需要在沙箱内执行命令、操作文件或管理沙箱生命周期时使用。
---

# rfb-cli 使用指南（AI 沙箱操作手册）

## 前置条件

- forkd-controller 已在宿主机运行（默认 `http://127.0.0.1:8889`，无认证）。
- 快照已创建且状态为 ready+bootable（`rfb-cli forkd snapshot-info --tag <tag>` 确认）。
- guest 内 forkd-agent 正常运行（快照创建时自动嵌入）。

## 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `FORKD_URL` | `http://127.0.0.1:8889` | forkd controller 地址 |
| `FORKD_TOKEN` | （空） | Bearer token（controller 未启用认证时不需要） |
| `FORKD_KERNEL` | `resx/kernel/vmlinux-arcbox-0.0.24` | 快照创建的内核路径 |
| `FORKD_ROOTFS` | `resx/rootfs/forkd-agent.ext4` | 快照创建的 rootfs 路径 |
| `FORKD_BIN` | `resx/forkd/forkd` | 官方 forkd 二进制路径 |

## 快速流程

### 1. 宿主预检

```bash
rfb-cli doctor --json
rfb-cli forkd preflight --tag <snapshot-tag>
```

### 2. 创建快照

```bash
rfb-cli forkd snapshot-create --tag my-snap --tap forkd-tap0
# 等待 status=ready && bootable=true（daemon 周期性扫描，约 300s）
rfb-cli forkd snapshot-info --tag my-snap
```

### 3. 创建沙箱

```bash
rfb-cli forkd sandbox-create --tag my-snap
# 输出 JSON 包含 sandbox id 和 guest_addr
```

### 4. 在沙箱内执行命令

```bash
# 通过 SDK 或 controller HTTP API:
# POST /v1/sandboxes/{id}/ping
# POST /v1/sandboxes {snapshot_tag, n, ...}
```

SDK 调用（四语言统一 API）见 `sdk/UNIFIED_API.md`。

### 5. 读写文件

```bash
# 通过 SDK 的 read/write 方法（≤ 50 KiB per request，offset 分页读取大文件）
```

### 6. 删除沙箱

```bash
rfb-cli forkd sandbox-destroy --id <sandbox-id>
```

## 退出码

| 码 | 含义 |
|---|---|
| 0 | 成功（或可选 VM 检查跳过） |
| 2 | 用法/参数错误 |
| 3 | 验证/协议失败 |
| 4 | I/O 错误 |
| 5 | 外部工具失败 |
| 12 | 必需 VM 前置条件不可用 |

## 常见错误

| 错误 | 原因 | 处置 |
|---|---|---|
| `guest connect refused` | guest agent 未启动或端口错误 | 检查快照 rootfs 内 forkd-agent 是否运行 |
| `snapshot not ready` | daemon 异步收编快照需时间 | 轮询 snapshot-info 等待 ready+bootable |
| `pty is not supported` | ZBRT/NDJSON 均不支持 PTY | 使用 stream + stdin 交互 |
| `args must not be empty` | exec argv 为空 | 传入非空命令数组 |
| `path escapes workspace` | 路径含 `..` 或绝对路径越界 | 改用 /workspace 内的相对路径 |
| `Connection refused`（controller） | forkd-controller 未启动 | 先启动 controller：`forkd-controller serve --bind 127.0.0.1:8889` |

## 传输选择

| 特性 | NDJSON (TCP) | ZBRT (vsock) |
|---|---|---|
| exec stdin | ✗（静默丢弃） | ✓ |
| eval | ✓ | ✓（通过 Execute argv=["eval",code]） |
| stream/交互 | ✓（started/out/err/exit） | ✓（Output/Exit 帧） |
| 文件读写 | ✓ | ✓（Fs 帧） |
| PTY | ✗ | ✗ |
