# SDK 后端准备指南：在 WSL / 真实 Linux 上用 rfb-cli 启动沙箱控制器

本指南教 SDK 用户从零把后端跑起来，再用 `RfbClient` 连上去。权威文档：`requirements/RFB/0.1.0/rfb-cli-usage.md`（CLI 合同）、`real-machine-runbook.md`（真机手册）、`image-build-runbook.md`（镜像手册）。

> **从零一条龙已实测**：本文件的 §0 在一台全新 Debian 13 WSL2（无任何开发环境）上
> 验证通过——zeroboot 冷启动基准、zeroboot 快照热启动、forkd `workload` 门禁
> （3 沙箱 / 129 ops / 0 orphan）全部跑绿，全靠 `rfb-cli` + 仓库自带的 `resx/` 资产。

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

## 0. 从零一条龙（全新 Debian，2026-09 实测）

前置：一台干净 Debian 12/13（WSL2 或真机），能访问外网。以下全部命令以 root 或
kvm 组用户执行（firecracker 需要 `/dev/kvm`；WSL2 默认提供）。

```bash
# 1) 系统依赖（约 1 分钟）
apt-get update && apt-get install -y --no-install-recommends \
  curl ca-certificates build-essential pkg-config e2fsprogs musl-tools git

# 2) Rust 工具链 + guest 静态目标（MSRV 1.93；musl target 必须显式添加，
#    `image build-static` 会 fail-closed 提示）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source ~/.cargo/env
rustup target add x86_64-unknown-linux-musl

# 3) 取仓库（含 resx/ 资产：firecracker、kernel、forkd 二进制）
git clone --depth 1 http://github.com/Cricle/rfb.git && cd rfb
# 3b) 标准安装：把 firecracker（补丁版）与 forkd 二进制放进 PATH
#     （controller 按自己进程的 PATH 查找 firecracker，rfb-cli 按同一
#     PATH 与 FORKD_* 查找；装进 /usr/local/bin 一次性解决，无 PATH 技巧）
cp resx/firecracker/firecracker-v1.12.1 /usr/local/bin/firecracker
cp resx/forkd/forkd /usr/local/bin/forkd
chmod +x /usr/local/bin/firecracker /usr/local/bin/forkd

# 4) 构建 rfb-cli（约 4 分钟）并体检
cargo build --release -p rfb-sdk --features cli     # 产物 target/release/rfb-cli
rfb-cli doctor --json                               # kvm/mke2fs/debugfs/cargo 应全 true

# 5) 静态 guest runtime + 两种 rootfs（各自约 1.5 分钟）
rfb-cli image build-static --root . --target x86_64-unknown-linux-musl --package rfb-runtime
rfb-cli image build-rootfs target/x86_64-unknown-linux-musl/release/rfb-runtime \
    resx/rootfs/zeroboot-zbrt.ext4 --mode zeroboot-zbrt --force
rfb-cli image build-rootfs target/x86_64-unknown-linux-musl/release/rfb-runtime \
    resx/rootfs/forkd-agent.ext4 --mode forkd-agent --force

# 5b) 需要内嵌解释器（/bin/python3 = RustPython、/bin/lua = mlua 5.4 硬链接）
#     时：runtime 必须带 feature 构建，rootfs 用 --with-* 安装（构建时会校验
#     二进制真的含解释器，feature 没开会 fail-closed，而不是装死链接）。
#     编译约 15 分钟（vendored C 代码走 musl-gcc，build-static 自动接线）。
rfb-cli image build-static --root . --target x86_64-unknown-linux-musl \
  --package rfb-runtime --features cli,rustpython,mlua
rfb-cli image build-rootfs target/x86_64-unknown-linux-musl/release/rfb-runtime \
    resx/rootfs/zeroboot-full.ext4 --mode zeroboot-zbrt --force \
    --with-python --with-lua
# 离线扩展包：--py-site-dir <纯Python包目录> / --lua-lib-dir <纯Lua模块目录>
# （沙箱无网络，pip 不可用）；不带解释器的默认镜像 /bin 只有
# sh/bash/sleep/echo/true/false/netprobe/nproc。
#
# 镜像体积参考（-m 0 取消 5% 保留块；ZBRT 镜像另去 journal——
# 它是每沙箱私有可抛弃副本，写全走 tmpfs；forkd 的 /forkd-init.sh 是
# 指向 runtime 的硬链接，同 inode 零拷贝）：
#   zeroboot-zbrt（默认，无解释器）      ~5 MB
#   zeroboot-zbrt + python/lua + 站点包  ~23 MB
#   forkd-agent（默认，无解释器）        ~10 MB
#   forkd-agent + python/lua + 站点包    ~28 MB

# 5c) 推荐：build.rfb 声明式脚本一条命令出镜像（上面 5b 的封装）——
#     自动派生 cargo features、fail-closed 校验解释器、支持离线包/
#     预编译 Rust 应用/附加文件。参考脚本 docs/example-build.rfb。
rfb-cli image build-script docs/example-build.rfb   # 或在仓库根放 build.rfb 后
rfb-cli image build-script                          # 默认找 ./build.rfb
```

`build.rfb` 字段速览（完整注释版见 `docs/example-build.rfb`）：

```toml
schema = "rfb-build/v1"
mode = "zeroboot-zbrt"            # zeroboot-zbrt / forkd-agent / rfb-vsock
output = "out/my-sandbox.ext4"    # 相对脚本所在目录
force = true

[interpreters]
python = true                     # /bin/python3（内嵌 RustPython）
lua = true                        # /bin/lua（内嵌 mlua 5.4）

[packages]
py-site = "sites/py"              # → /usr/lib/python3/site-packages
lua-lib = "sites/lua"             # → /usr/lib/lua/5.4

[rust]
apps = ["hello"]                  # 预编译 musl 二进制 → /usr/local/bin/<名>

[files]
"assets/banner.txt" = "/etc/motd" # 宿主路径 → guest 绝对路径（0644）
```

> 已实测（全新 Debian + KVM 真机）：脚本一键出镜像后，guest 内
> `python3 -c "import demo"`、`lua -e "require('demo').answer()"`、
> `/usr/local/bin/hello`（Rust musl 应用）、`/etc/motd` 全部 PASS（zeroboot
> 与 forkd 两种模式都验证过）。解释器/站点包在**所有模式**可用（hardlink
> 源是各模式的 entrypoint，guest 侧按 argv[0] 分发）。

# 6) 校验内核
rfb-cli image check-kernel resx/kernel/vmlinux-arcbox-0.0.24
```

**内核选择矩阵**（实测结论，选错内核是最大坑）：

| 内核 | zeroboot 冷启动 | zeroboot 热启动 | forkd（需 virtio-net） | 获取 |
|---|---|---|---|---|
| `resx/kernel/vmlinux-arcbox-0.0.24`（仓库自带） | ✓ 最快（echo p50 ~0.9ms） | ✗ 恢复后 vsock 中断 kernel panic | ✗ 无 virtio-net，guest 网络不通 | 仓库自带 |
| `vmlinux-5.10.225` | ✓（稳态 ~1.3ms） | ✓ | ✓ | `curl -L https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.11/x86_64/vmlinux-5.10.225 -o resx/kernel/vmlinux-5.10.225` |
| `vmlinux-6.1.141` | ✓ | ✓（可用） | ✓ | `curl -L https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.13/x86_64/vmlinux-6.1.141 -o resx/kernel/vmlinux-6.1.141` |

**Firecracker 二进制**：仓库自带两个——`resx/firecracker/firecracker-v1.12.1`
（补丁版，DevPreview 快照、`PUT /actions`；zeroboot 热启动与 forkd 都要用它）与
`resx/firecracker/firecracker-v1.16.1-x86_64.tgz`（上游版，解压前先
`cd resx/firecracker && sha256sum -c firecracker-v1.16.1-x86_64.tgz.sha256`，
注意必须在 `resx/firecracker/` 目录内执行，否则按文件名找不到）。

### 6a) zeroboot 冷启动基准（真机验收 + 性能基线）

```bash
cargo build --release -p rfb-ben     # 基准工具（仓库内 crate，不走 cargo install）
rfb-ben zbrt \
  --kernel resx/kernel/vmlinux-arcbox-0.0.24 \
  --rootfs resx/rootfs/zeroboot-zbrt.ext4 \
  --firecracker resx/firecracker/firecracker-v1.12.1 \
  --samples 100
```

全新 Debian 实测参考值：cold_boot ~0.5-1.8s（冷页缓存更慢）、echo p50 ~0.4-0.9ms、
4 并发有效并发 ~3.9。

### 6b) zeroboot 热启动（快照恢复，免冷启动）

```bash
export RFB_ZBRT_SNAPSHOT_DIR=/dev/shm/rfb-zbrt-snap   # 建议 tmpfs
# 此后每个 create：首个 = boot 父 VM + pause + snapshot（一次性 ~1-6s），
# 之后每个 = 纯恢复（实测 ~200ms，5.10.225 内核）
```

- 内核**必须**用上表 5.10.225 / 6.1.141；arcbox 会在恢复后的 vsock 中断上 panic。
- 快照目录带身份指纹（kernel/rootfs/firecracker/shape/port），改配置自动重建。
- 内存走父快照 CoW 共享、rootfs 共享（guest 写落在 per-VM tmpfs），无 per-create 拷贝。
- 限制：resx 补丁版 firecracker 把设备路径烘焙进快照，热启动创建在单分片内串行；
  并发沙箱用分片（见下）或多节点集群（见 §7）扩吞吐。

#### 6b-2) 100 并发：快照分片

```bash
export RFB_ZBRT_SNAPSHOT_DIR=/dev/shm/rfb-zbrt-snap
export RFB_ZBRT_SNAPSHOT_SHARDS=4     # 默认已是 2；4 片把 100 并发摊薄到 ~54ms/个
export RFB_ZBRT_VM_MEM_MIB=48         # 2GB 机器档位；默认 128（Firecracker 下限 48）
```

**默认值即实测最佳**（本轮校准）：`VM_MEM_MIB=128`（下限 48，2.5x 余量）、
`SESSIONS=8`（1 vCPU 实测 16 会话有效并发 14.97）、`SHARDS=2`（默认把
100 并发摊薄到 ~106ms/个）。

真机实测（16 核 / 15GB WSL，release 档，`tests/zeroboot_concurrency.rs`）：
100 并发 create **100/100 成功**，摊薄 **~54ms/create**（4 分片）；随后 100 个
并发 echo 全部成功（~1s）。分片数按 CPU 预算（每分片第一个 create 付一次
父快照 ~1.1s）。首次创建会自动把快照目录收紧为 0700/0600（memory.bin 含
VM 内存，禁止 world-readable）。CI 门禁：ci.yml 的 e2e job
"Real-VM 100-concurrency gate" 步骤。

跑这两个真机测试（concurrency / fork）需要的环境变量（测试里都有默认值，
默认 rootfs 名是 e2e CI 产物 `resx/rootfs/zeroboot-zbrt-e2e.ext4`，本仓库
resx 里没有，所以**必须**传 `RFB_E2E_ZBRT_ROOTFS`）：

```bash
export RFB_REAL_E2E=1
export RFB_E2E_FIRECRACKER=/usr/local/bin/firecracker
export RFB_E2E_ZBRT_ROOTFS=$PWD/examples/full/out/full-sandbox.ext4   # 必须绝对路径
export RFB_E2E_KERNEL=$PWD/resx/kernel/vmlinux-5.10.225               # 必须绝对路径（CWD 在 crate 下）
export RFB_ZBRT_SNAPSHOT_DIR=/dev/shm/rfb-zbrt-snap RFB_ZBRT_SNAPSHOT_SHARDS=4
export RFB_ZBRT_VM_MEM_MIB=64
export RFB_E2E_CONCURRENCY=100      # 测试默认 25（CI 用）；本地复现 100 需显式设置
cargo test -p rfb-sdk --features cli --release --test zeroboot_concurrency -- --ignored
cargo test -p rfb-sdk --features cli --release --test zeroboot_fork -- --ignored
```

#### 6b-3) 内存驻留模型（按需加载，不整块进 RAM）

- **恢复即 mmap**：Firecracker 载入 `memory.bin` 用 `MAP_PRIVATE` 映射——内核
  按缺页按需加载，百个子 VM 与父快照 CoW 共享同一批物理页，创建时无整块拷贝
  （100×64MiB 实测总驻留 ~7GB，远小于 100 份独立内存的 12.8GB）。
- **快照目录落盘 vs tmpfs**：放磁盘（默认 `/var/lib/...` 类路径）时 `memory.bin`
  由页缓存按需换入，宿主常驻内存最小（"用时加载"最彻底）；放 tmpfs
  （`/dev/shm`）时页天然驻留 RAM，恢复更快。100 并发大规模部署建议磁盘 +
  充足内存，或 tmpfs + 更小 `RFB_ZBRT_VM_MEM_MIB`。
- **构建期校验流式化**：镜像安装校验（runtime/站点包/附加文件逐字节比对）与
  解释器特征扫描按 64KiB 分块边读边比，任何负载都不整读进内存
  （`verify_installed_file` / `binary_contains`）。

#### 6b-4) 沙箱 fork（checkpoint 分叉，对齐 forkd live-fork）

`ZeroBootSandbox::fork(self)` 把一个活动沙箱 checkpoint（pause → 全量快照）
后恢复出**两个**全新 VM，返回 `(原沙箱的延续, fork)`——两者从同一状态出发：
workspace tmpfs 是 guest 内存的一部分，全量 dump 把它整体捕获，所以 fork
继承 fork 时刻的全部文件/进程/内存状态（真机实测：写进 workspace 的文件
在 fork 与 fork 的 fork 里都能读到；三代沙箱都可继续服务）。

```rust
let (original, fork) = original.fork().await?;   // 原句柄由 fork 消费
let (fork, grandchild) = fork.fork().await?;     // 链式 fork 同样可用
```

- **代价**：补丁版 Firecracker 只有全量 dump（无 forkd 的增量 diff），一次
  fork ≈ checkpoint(~40ms) + 两次 restore(~400ms)。
- **为什么原 VM 会被替换**：checkpoint 会重置 guest 的活动 vsock 连接，且
  补丁版把同一 relay 路径烘焙进所有快照——fork 直接把两个连接完好的新 VM
  交还给调用方，避免"原名被抢、原句柄失联"的灰色状态。
- **生命周期**：checkpoint 目录（含 CoW 背板的 memory.bin）由两个子沙箱
  共享持有，最后一个 drop 时自动删除（`fork_checkpoint_dir()` 可查）。
- 需要 `RFB_ZBRT_SNAPSHOT_DIR`（热模式）；冷启动 VM 的 relay 路径在私有
  tempdir 里，随原沙箱消亡，fork 不支持。
- 真机门禁：`rfb/tests/zeroboot_fork.rs`（ci.yml 的 e2e job "Real-VM fork gate" 步骤）；
  单元门禁：fork 前置条件 fail-closed（`zeroboot_session.rs`）。

### 6c) forkd（root 权限，TAP 网络）

```bash
# TAP + 转发（需要 root；forkd 沙箱默认共享 forkd-tap0，同一时刻只允许
# 一个活沙箱，并行需 per_child_netns=true 或 netns-setup）。
# WSL 各发行版共享内核，TAP 是内核级设备：已存在时必须复用而不是重建
# （否则 ioctl(TUNSETIFF): Device or resource busy），所以先判断再建。
if ! ip link show forkd-tap0 >/dev/null 2>&1; then
  ip tuntap add dev forkd-tap0 mode tap
fi
ip addr show forkd-tap0 | grep -q 10.42.0.1/24 || ip addr add 10.42.0.1/24 dev forkd-tap0
ip link set forkd-tap0 up
sysctl -w net.ipv4.ip_forward=1

# controller（前提：firecracker/forkd 已装入 /usr/local/bin，§0 步骤 3b；
# controller 按自己进程启动时的 PATH 查找 firecracker）。
# 注意：每条 wsl 命令是独立会话，裸 `&` 起的进程会随会话退出被杀——必须
# setsid nohup 脱会话，并把 stdout/stderr 落盘以便排障。
mkdir -p /tmp/rfb-live
setsid nohup resx/forkd/forkd-controller serve \
  --state /tmp/rfb-live/state.json --audit-log /tmp/rfb-live/audit.log \
  --snapshot-root /root/.local/share/forkd/snapshots --bind 127.0.0.1:8889 \
  > /tmp/rfb-live/stdout.log 2>&1 < /dev/null &

# 快照（内核/rootfs 用旗标或环境变量传给 rfb-cli，它校验后委托官方 forkd）
rfb-cli forkd snapshot-create --tag fresh --tap forkd-tap0 \
  --kernel resx/kernel/vmlinux-5.10.225 --rootfs resx/rootfs/forkd-agent.ext4
rfb-cli forkd preflight --tag fresh        # 缺什么会直接点名（非登录会话记得先 source ~/.cargo/env）
rfb-cli forkd workload --url http://127.0.0.1:8889 --tag fresh --json   # 业务门禁
rfb-cli forkd acceptance --tag fresh --require-vm --json                # 完整协议验收

# guest 内执行命令走**官方 forkd 二进制**（rfb-cli 没有 exec 子命令）：
#   forkd exec --target <guest_addr> -- <cmd>
ADDR=$(curl -s http://127.0.0.1:8889/v1/sandboxes | python3 -c "import json,sys;print(json.load(sys.stdin)[0]['guest_addr'])")
forkd exec --target "$ADDR" -- /usr/local/bin/hello rfb
```

全新 Debian 实测：workload PASS（3 沙箱、ping/exec/write/read/find/grep/stream
共 129 ops、0 orphan）；`build.rfb` 自定义 forkd 镜像（解释器 + Rust 应用 + 站点包）
的 acceptance 全门禁 PASS（9 正向 + 3 负向），guest 内 python/lua/hello 输出正常。
沙箱创建 ~90-110ms（快照恢复）。

### 7) 分布式 / 集群沙箱（ClusterProvider）

每台节点机就是一台标准 forkd controller（§6c），集群层不引入新服务端组件：
`rfb::cluster::ClusterProvider` 在 N 个 controller 端点之间调度 sandbox 创建——
默认最少在途数（LeastInflight）、可选轮询（RoundRobin）。传输失败、5xx、
响应不可解析（Decode）按「节点故障」计一次失败并自动 failover 到其余节点，
连续 3 次失败熔断；4xx 是节点明确拒绝，立即向调用方报错、不计失败也不转移
（避免双创建）。熔断节点默认每 30 秒最多放行一次惰性半开试探
（`recovery_interval`，默认 30s；试探 create 成功即恢复），`probe()` 可立即手动
巡检（不计失败，失败只推迟下次试探）。传输/解析失败后还会对账该节点的沙箱
列表：标签匹配且 `created_at_unix` 落在本次尝试窗口内**恰好一条**时删除该孤儿，
其余（0 条 / 多条 / 无时间戳 / 列表失败）只告警不动（宁漏不误）。
exec/stream/文件操作直连沙箱 guest 地址（不过 controller），销毁路由回创建
节点。create 返回的 handle 用 RAII 释放在途计数，`drop` 即释放（无需手工
release）。所有节点必须预置同一个 snapshot tag。

```rust
use rfb::cluster::{ClusterConfig, ClusterProvider};
use rfb::{SandboxProvider, ExecSpec};
use std::time::Duration;

let provider = ClusterProvider::new(ClusterConfig::from_urls(
    ["http://10.0.0.1:8889", "http://10.0.0.2:8889", "http://10.0.0.3:8889"],
    "fresh", // 各节点已就绪的快照 tag
).with_recovery_interval(Duration::from_secs(30)))?; // 默认即 30s
let sandbox = provider.create(Default::default()).await?; // 调度到负载最低节点
let result = sandbox.exec(ExecSpec::new("uname")).await?;
drop(sandbox);                         // RAII：在途计数随 handle 释放，无需手工 release
provider.probe().await;                // 手动巡检（成功即恢复熔断节点）
let status = provider.node_statuses(); // healthy/tripped/failures/inflight/next_trial，接监控

// 上线前跨节点一致性：所有节点 ready/bootable，且快照 digest 一致
let report = provider.preflight().await;
assert!(report.digests_agree, "snapshot digest mismatch: {:?}", report.notes);

// 属主宕机后的孤儿清理：list_all 聚合各节点 → delete_on 精确清理
for node in provider.list_all().await {
    for sandbox in &node.sandboxes { /* 判断 handle 已丢失的孤儿 */ }
}
provider.delete_on("http://10.0.0.1:8889", "sbx-123").await?;
```

- 节点扩容：加 URL 即可；沙箱 handle 自带所属节点，无需集群级会话粘性。
- 100 并发集群 = 每节点 §6b-2 的分片并发再叠加节点间调度。
- 单测：`rfb/tests/cluster.rs`（mock controller 覆盖调度/错误分级/熔断/半开/
  对账/预检/聚合清理）。

## 1. 前提条件

- **WSL2 或真实 Linux**（x86_64）。不要用 Windows `.exe` 当 Linux 服务；没有 Linux 二进制就先在 WSL 里构建。
- Rust toolchain（MSRV 1.93+，见各 crate 的 rust-version）。
- 需要 VM 后端时：`/dev/kvm` 可用，且匹配的 Firecracker、kernel、rootfs（仓库已带 `firecracker-v1.16.1-x86_64.tgz` 与 `vmlinux-arcbox-0.0.24`）。
- 创建 TAP 网卡需要 root（`ip tuntap`）；若 `cni0` 已占用 `10.42.0.1/24`，先记录现状再临时挪址，结束后恢复。

## 2. 构建 rfb-cli

```bash
cd monitor/rfb
cargo build -p rfb-sdk --features cli    # 产物: target/debug/rfb-cli
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

controller 是 forkd 官方二进制（`resx/forkd/forkd-controller`），rfb 不复制其内部实现。用独立 state / audit log 启动（子命令是 `serve`）：

```bash
# 前提：firecracker 与 forkd 已装入 /usr/local/bin（§0 步骤 3b）——
# controller 按自己进程的 PATH 查找 firecracker，装进标准路径即无 PATH 技巧。
mkdir -p /tmp/rfb-live
resx/forkd/forkd-controller serve \
  --state /tmp/rfb-live/controller/state.json \
  --audit-log /tmp/rfb-live/controller/audit.log \
  --snapshot-root /root/.local/share/forkd/snapshots \
  --bind 127.0.0.1:8889 &
echo $! > /tmp/rfb-live/controller/pid

curl -s http://127.0.0.1:8889/v1/snapshots   # 应返回 []（空池）
```

> 具体旗标以你手上的 forkd 版本 `forkd-controller serve --help` 为准；要点是：
> **`serve` 子命令 + 独立 state 文件 + snapshot root 指向快照目录 + 监听 8889**。
> controller 与它 fork 出的沙箱都读取启动时的 PATH。

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
| `FORKD_KERNEL` | snapshot 创建用的 vmlinux 内核路径（默认 `resx/kernel/vmlinux-arcbox-0.0.24`）。**forkd 必须覆盖成带 virtio-net 的内核**（5.10.225/6.1.141，见 §0 矩阵）——arcbox 无 virtio-net，建出的快照 guest 永不就绪 | `rfb-cli forkd snapshot-create`（`--kernel` 未传时） |
| `FORKD_ROOTFS` | snapshot 创建用的 forkd-agent rootfs 路径（默认 `resx/rootfs/forkd-agent.ext4`；boot 前复制为快照私有副本） | `rfb-cli forkd snapshot-create`（`--rootfs` 未传时） |
| `FORKD_BIN` | 委托的官方 forkd 二进制路径（默认 `resx/forkd/forkd`，再退到 PATH；装进 `/usr/local/bin` 后无需设置） | `rfb-cli forkd snapshot-create / snapshot-info / snapshot-delete` 等 |
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
| `image build-static` 报缺 musl target | `rustup target add x86_64-unknown-linux-musl` |
| 带 `--features rustpython,mlua` 构建报 `undefined reference to errno/_dl_x86_cpu_features` | C 代码被宿主 glibc 污染——新版 build-static 已自动注入 `CC_*_MUSL=musl-gcc` 与 `CARGO_TARGET_*_LINKER=musl-gcc`（旧版需手工 export） |
| `build-rootfs --with-python` 报 "require the runtime built with the rustpython feature" | 二进制里没有解释器（feature 没编进去）；这是新版 fail-closed 校验——防止装一个 guest 里跑不起来的死链接 /bin/python3 |
| `sha256sum -c` 报找不到 tgz | 必须在 `resx/firecracker/` 目录内执行（校验文件按文件名引用） |
| forkd snapshot-create 报 "failed to spawn firecracker" / "forkd CLI binary not found" | `firecracker` / `forkd` 不在 PATH 上——按 §0 步骤 3b 装进 `/usr/local/bin`（controller 查找用的是**它自己进程启动时**的 PATH） |
| `rfb-cli forkd preflight` 报 FORKD_BIN/FORKD_ROOTFS blocked | `rfb-cli forkd *` 子命令也读这些环境变量（见 §6c 的 export），preflight 会逐项点名 |
| forkd 沙箱建出但 guest 永不就绪 | 内核不带 virtio-net（resx 的 arcbox 就没有）——forkd 必须用 5.10.225/6.1.141 |
| zeroboot 热启动后 guest 立即失联/panic | 内核不支持恢复后的 vsock（arcbox 会 "Fatal exception in interrupt"）——热启动用 5.10.225/6.1.141 |
| 恢复后首个 exec 明显慢（~100ms） | 恢复后首次冷执行（COW 触页 + 页缓存）属一次性成本，预热后的稳态为毫秒级；用 `rfb-ben` 看预热后数字 |
| 快照一直不 ready | 看 controller audit log；`snapshot-info` 查 status=failed 原因 |
| `forkd-tap0` 创建失败 | 需要 root；`cni0` 占用 10.42.0.1/24 时先挪址（结束记得恢复） |
| 删沙箱后再建卡 10 秒 | controller 对"孤儿 firecracker 进程"有 10s 等待；确认旧 VM 已退出或重启 controller |
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
