# SDK 后端准备指南：在 WSL / 真实 Linux 上用 rfb-cli 启动沙箱控制器

本指南教 SDK 用户从零把后端跑起来，再用 `RfbClient` 连上去。权威文档：`requirements/RFB/0.1.0/rfb-cli-usage.md`（CLI 合同）、`real-machine-runbook.md`（真机手册）、`image-build-runbook.md`（镜像手册）。

```text
你的程序（RfbClient，任意语言）
  │  HTTP/JSON  FORKD_URL（默认 http://127.0.0.1:8889）
  ▼
forkd controller（沙箱生命周期：快照/创建/销毁）
  │  返回每个沙箱的 guest_addr
  ▼
沙箱 guest（Linux VM / 容器）
  ├─ forkd guest agent（TCP 8888，NDJSON） ← Sandbox 默认传输
  └─ ZeroBoot guest（vsock 5000 → TCP relay，ZBRT 帧） ← transport="zbrt"
```

## 1. 前提条件

- **WSL2 或真实 Linux**（x86_64）。不要用 Windows `.exe` 当 Linux 服务；没有 Linux 二进制就先在 WSL 里构建。
- Rust toolchain（MSRV 1.82+）。
- 需要 VM 后端时：`/dev/kvm` 可用，且匹配的 Firecracker、kernel、rootfs（仓库已带 `firecracker-v1.16.1-x86_64.tgz` 与 `vmlinux-arcbox-0.0.24`）。
- 创建 TAP 网卡需要 root（`ip tuntap`）；若 `cni0` 已占用 `10.42.0.1/24`，先记录现状再临时挪址，结束后恢复。

## 2. 构建 rfb-cli

```bash
cd monitor/rfb
cargo build -p rfb --features cli        # 产物: target/debug/rfb-cli
rfb-cli doctor --json                    # 检查宿主能力（kvm/mke2fs/网络工具）
```

构建报 `openssl-sys` 缺依赖 → 安装 `libssl-dev` 和 `pkg-config` 后重试（Debian/Ubuntu）。

## 3. 资源与镜像

资源目录约定（`resx/`）：

```text
rfb/resx/
├── forkd/        # forkd 官方二进制（controller + guest agent）
├── firecracker/  # Firecracker 二进制（sha256sum -c firecracker-v1.16.1-x86_64.tgz.sha256）
├── kernel/       # vmlinux-arcbox-0.0.24
├── rootfs/       # 各协议 rootfs（不混用！）
└── snapshots/    # 快照输出
```

构建静态 runtime binary 和 rootfs（**SDK 的两种传输对应两种 mode**）：

```bash
# 1) 静态编译 rfb-runtime（musl）
rfb-cli image build-static --root . --target x86_64-unknown-linux-musl --package rfb-runtime

# 2a) forkd-agent rootfs → Sandbox 默认传输（NDJSON，guest port 8888）
rfb-cli image build-rootfs target/x86_64-unknown-linux-musl/release/rfb-runtime \
    resx/rootfs/forkd-agent.ext4 --mode forkd-agent --force

# 2b) zeroboot-zbrt rootfs → transport="zbrt"（ZBRT 帧，vsock 5000）
rfb-cli image build-rootfs target/x86_64-unknown-linux-musl/release/rfb-runtime \
    resx/rootfs/zeroboot-zbrt.ext4 --mode zeroboot-zbrt --force

# 校验 kernel
rfb-cli image check-kernel resx/kernel/vmlinux-arcbox-0.0.24
```

**一条命令（all-in-one）**：静态编译（含嵌入解释器 feature）→ 组装 rootfs（自动装
`/bin/python3`、`/bin/lua` 硬链接与离线扩展包）→ 校验 kernel → Firecracker 真机 boot 验证：

```bash
rfb-cli image build-all \
  --root . --target x86_64-unknown-linux-musl \
  --features cli,rustpython,mlua \
  resx/rootfs/zeroboot-zbrt.ext4 \
  --mode zeroboot-zbrt --force \
  --py-site-dir ~/rfb-sites/py --lua-lib-dir ~/rfb-sites/lua \
  --kernel resx/kernel/vmlinux-arcbox-0.0.24
```

- `--features` 决定嵌入解释器：`rustpython` → 装 `/bin/python3`，`mlua` → 装 `/bin/lua`，可任选/全要/都不要（默认 shell）。
- `--py-site-dir` / `--lua-lib-dir` 把本地**纯 Python / 纯 Lua** 包目录打进镜像（沙箱无网络，`pip` 不可用；Python 无 ssl/multiprocessing/ctypes/sqlite3）。
- `--kernel` 省略时跳过第 3/4 阶段，只出 rootfs。

`rfb-vsock` / `forkd-agent` / `zeroboot-zbrt` 三种 rootfs **不可互换**。

## 4. 启动 forkd controller

controller 是 forkd 官方二进制（`resx/forkd/forkd-controller`），rfb 不复制其内部实现。用独立 state / audit log / pid 文件启动：

```bash
mkdir -p /tmp/rfb-live
resx/forkd/forkd-controller \
  --state-dir /tmp/rfb-live/controller/state \
  --audit-log /tmp/rfb-live/controller/audit.log \
  --snapshot-root ~/.local/share/forkd/snapshots \
  --listen 127.0.0.1:8889 &
echo $! > /tmp/rfb-live/controller/pid

curl -s http://127.0.0.1:8889/v1/snapshots   # 应返回 []（空池）
```

> 具体旗标以你手上的 forkd 版本 `--help` 为准；要点是：**独立 state 目录 + snapshot root 指向快照目录 + 监听 8889**。

设置 SDK 用的环境变量：

```bash
export FORKD_URL=http://127.0.0.1:8889
# export FORKD_TOKEN=<token>          # 仅当 controller 配置了认证
```

## 5. 环境变量

SDK 与 `rfb-cli` 实际读取的环境变量一览（以代码为准）：

| 变量 | 覆盖什么 | 谁读取 |
|---|---|---|
| `FORKD_URL` | forkd controller 地址（默认 `http://127.0.0.1:8889`） | 四语言 SDK 的 `RfbClient` 缺省构造；`rfb-cli forkd *` 各子命令 |
| `FORKD_KERNEL` | snapshot 创建用的 vmlinux 内核路径（默认 `resx/kernel/vmlinux-arcbox-0.0.24`） | `rfb-cli forkd snapshot-create`（`--kernel` 未传时） |
| `FORKD_ROOTFS` | snapshot 创建用的 forkd-agent rootfs 路径（默认 `resx/rootfs/forkd-agent.ext4`；boot 前复制为快照私有副本） | `rfb-cli forkd snapshot-create`（`--rootfs` 未传时） |
| `FORKD_BIN` | 委托的官方 forkd 二进制路径（默认 `resx/forkd/forkd`，再退到 PATH） | `rfb-cli forkd snapshot-create / snapshot-info / snapshot-delete` 等 |
| `RFB_RUNTIME_BIN` | 预编译静态 rfb-runtime 二进制的注入点（部署流水线按平台注入外部制品的约定名；仓库内代码不读它，构建时该路径作为 `rfb-cli image build-rootfs` 的位置参数传入） | 仓库文档约定的制品注入方 / 部署流水线 |
| `RFB_AGENT_WORKSPACE` | forkd agent 的 guest 工作区根（默认 `/workspace` tmpfs） | rfb-runtime agent（`rfb-runtime/src/agent/transport.rs` 的 `workspace_root()`）——供宿主侧契约测试在无法创建 `/workspace` 时重定向 |

## 6. 创建快照并等就绪

`snapshot-create` 是 `rfb-cli` 薄封装：kernel/rootfs/forkd 二进制从 `resx/` 或环境变量（`FORKD_KERNEL` / `FORKD_ROOTFS` / `FORKD_TAP` / `FORKD_BIN`）解析校验后委托官方 `forkd`。rootfs 会被复制为快照私有副本再 boot（原件保持 pristine）。

```bash
rfb-cli forkd snapshot-create \
  --tag rfb \
  --kernel resx/kernel/vmlinux-arcbox-0.0.24 \
  --rootfs resx/rootfs/forkd-agent.ext4

# 就绪/可引导以 list 为准（轮询直至 status=ready 且 bootable=true）
rfb-cli forkd snapshot-info --tag rfb
rfb-cli forkd preflight --tag rfb          # 只读预检
rfb-cli forkd acceptance --tag rfb --require-vm   # 完整验收（可选但推荐）
```

## 7. 从 SDK 连接

四语言同一套流程：建沙箱 → exec → 文件读写 → 删除。

```python
from rfb_sdk import RfbClient          # FORKD_URL/FORKD_TOKEN 自动读取

rfb = RfbClient()
rfb.wait_snapshot("rfb")               # 双保险：等快照就绪
sbx = rfb.create_sandbox("rfb")[0]
r = sbx.exec(["echo", "hello"], timeout_s=30)
print(r.exit_code, r.stdout_text)      # 0 hello
sbx.write("/workspace/a.txt", b"hi")
print(sbx.read("/workspace/a.txt").data)
sbx.delete()
```

```csharp
var rfb = new RfbClient();             // 读 FORKD_URL / FORKD_TOKEN
await rfb.WaitSnapshot("rfb");
var sbx = (await rfb.CreateSandbox("rfb"))[0];
var r = await sbx.Exec(new[]{"echo","hello"}, timeoutS: 30);
Console.WriteLine(r.ExitCode + " " + r.StdoutText);
sbx.Delete();
```

```java
RfbClient rfb = new RfbClient();
rfb.waitSnapshot("rfb");
Sandbox sbx = rfb.createSandbox("rfb").get(0);
ExecResult r = sbx.exec(List.of("echo","hello"), "/", 30.0, new byte[0]);
sbx.delete();
```

```rust
use rfb::client::RfbClient;
let rfb = RfbClient::from_env().await?;
rfb.wait_snapshot("rfb", std::time::Duration::from_secs(60)).await?;
let sbx = rfb.create_sandbox("rfb", Default::default()).await?.remove(0);
let r = sbx.exec(&["echo".into(), "hello".into()], "/", std::time::Duration::from_secs(30), b"").await?;
sbx.delete().await?;
```

**Windows 宿主连 WSL2 里的 controller**：WSL2 的 `localhost` 默认自动转发到 Windows；不通时用 `wsl hostname -I` 取 WSL IP，把 `FORKD_URL` 指到 `http://<WSL_IP>:8889`，或在 controller 侧监听 `0.0.0.0`。反向（WSL 连 Windows）用 Windows 宿主 IP（`/etc/resolv.conf` 里的 nameserver 或 `ip route show default`）。

**transport="zbrt"**：把上面的 rootfs 换成 `zeroboot-zbrt.ext4` 建快照，创建/连接沙箱时传 `transport="zbrt"`（Rust 用 `GuestTransport::Zbrt`），其余代码完全不变。

## 8. 故障排查

| 现象 | 处理 |
|---|---|
| `doctor` 报缺 kvm/mke2fs | WSL2 需嵌套虚拟化启用；安装 e2fsprogs |
| 快照一直不 ready | 看 controller audit log；`snapshot-info` 查 status=failed 原因 |
| `forkd-tap0` 创建失败 | 需要 root；`cni0` 占用 10.42.0.1/24 时先挪址（结束记得恢复） |
| SDK 报 HttpStatusError | `curl $FORKD_URL/v1/snapshots` 先手动确认 controller 活着 |
| SDK 报 RemoteError | 看 guest 侧响应；`rfb-cli forkd acceptance --tag <tag>` 复现完整链路 |
| 构建报 openssl-sys | `apt install libssl-dev pkg-config` |
| 退出码 12 | 必需的 VM/前置条件缺失，fail closed |

退出码表：0 成功 / 2 用法错 / 3 校验错 / 4 I/O 错 / 5 外部工具错 / 12 缺 VM 或前置。

## 9. 清理与安全

```bash
kill "$(cat /tmp/rfb-live/controller/pid)"
forkd rmi rfb-live                      # 只删本轮专用 tag
ip link set forkd-tap0 down; ip tuntap del dev forkd-tap0 mode tap
rm -rf /tmp/rfb-live
```

- token/API key 只经环境变量进入进程，不写入命令行、日志、报告。
- 不删除他人 controller 的 state/snapshot；清理后确认无遗留进程与 TAP。
- 低等级证据（fake/LOCAL_CONTRACT）不能升格为真实 KVM 验收结论。
