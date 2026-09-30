# RFB 沙箱镜像 Cookbook（build.rfb 示例集）

本目录是一套可直接照抄的"从零到带库镜像"示例。每个示例都是一份
`build.rfb` 声明式脚本 + 它引用的资源；构建后即可用 zeroboot（热启动/分片
并发/fork）或 forkd（快照/分支）启动真机验证。**从零起步**（全新
Debian/WSL 装依赖、装工具链、构建镜像到跑通全功能）见
[`sdk/BACKEND_SETUP.md`](../sdk/BACKEND_SETUP.md) §0——本文件只讲镜像
本身。

## 快速开始（一分钟版）

```bash
cd rfb
rfb-cli image build-script examples/full/full.rfb
# 产物: examples/full/out/full-sandbox.ext4（解释器构建约 15 分钟，含 vendored C）
```

## 示例列表

| 示例 | 演示内容 |
|---|---|
| [`full/full.rfb`](full/full.rfb) | 最完整（zeroboot）：python + lua 双解释器、离线站点包（`sites/demo/__init__.py`、`sites/demo.lua`，与 `zeroboot_interpreters` 真机测试的断言对齐）、预编译 Rust 应用（`hello-app/`）、附加文件（`assets/banner.txt` → `/etc/motd`） |
| [`full/full-forkd.rfb`](full/full-forkd.rfb) | 同上一套载荷的 forkd 模式镜像（forkd 沙箱同样带解释器与站点包） |
| [`minimal-forkd.rfb`](minimal-forkd.rfb) | 最小 forkd 镜像：只装 Rust 应用 + 附加文件（无解释器，构建最快） |

## 带库镜像的约定

- **Python**：`[packages] py-site = "..."` 目录下的每个 `.py` 会被烤进镜像的
  `/usr/lib/python3/site-packages`（guest 的 `sys.path` 根）。沙箱无网络，
  只能烤**纯 Python**包（无 C 扩展）。示例里的 `sites/demo/__init__.py`
  就是 `zeroboot_interpreters` 真机测试导入的模块（断言 `VALUE == 42`）。
- **Lua**：`[packages] lua-lib = "..."` 目录下的模块烤进 `/usr/lib/lua/5.4`
  （`package.path` 根），同样只支持纯 Lua。示例的 `sites/demo.lua` 对应
  真机测试的 `require('demo').answer() == 42`。
- **Rust 应用**：`[rust] apps = [...]` 只接受**已编译的 musl 静态二进制**，
  安装为 `/usr/local/bin/<文件名>`（0755）。编译方式（`hello-app/` 是源码，
  构建产物 `hello` 是二进制文件——注意产物不要与源码目录同名）：

  ```bash
  cd examples/full
  cargo build --release --target x86_64-unknown-linux-musl \
    --target-dir target --manifest-path hello-app/Cargo.toml
  cp target/x86_64-unknown-linux-musl/release/hello hello
  ```

## 真机验证（构建出的镜像可以直接跑）

内核选择：`vmlinux-5.10.225`（forkd 必须；zeroboot 热启动/fork 必须用
5.10.225/6.1.141——arcbox 内核会在恢复的 vsock 上 panic；zeroboot **纯冷
启动**可以用 resx 的 arcbox）。

```bash
# zeroboot：解释器 + Rust 应用真机套件（测试在 tests/ 文件夹）
RFB_REAL_E2E=1 RFB_E2E_FIRECRACKER=firecracker \
  cargo test -p rfb-sdk --features cli --test zeroboot_interpreters \
  -- --ignored --test-threads=1

# zeroboot 热启动 + 分片 100 并发 + fork（真机门禁）
export RFB_ZBRT_SNAPSHOT_DIR=/dev/shm/rfb-zbrt-snap RFB_ZBRT_SNAPSHOT_SHARDS=4
export RFB_ZBRT_VM_MEM_MIB=64 RFB_E2E_KERNEL=resx/kernel/vmlinux-5.10.225
cargo test -p rfb-sdk --features cli --release --test zeroboot_concurrency -- --ignored
cargo test -p rfb-sdk --features cli --release --test zeroboot_fork -- --ignored

# 容量基准：冷启动 / 往返 / 并发阶梯 / vCPU×内存阶梯 / Firecracker RSS
cargo build --release -p rfb-ben
rfb-ben zbrt --kernel resx/kernel/vmlinux-5.10.225 \
  --rootfs examples/full/out/full-sandbox.ext4 \
  --firecracker firecracker --samples 100 --warmup 5 \
  --levels 1,2,4,8,16 --rounds 4 --mem-mib 512,256,64 --vcpus 1,2

# forkd（TCP，需要 TAP + controller，见 sdk/BACKEND_SETUP.md §6c）
rfb-cli forkd snapshot-create --tag demo \
  --kernel resx/kernel/vmlinux-5.10.225 \
  --rootfs examples/full/out/full-forkd.rfb 的产物 my-forkd.ext4
rfb-cli forkd acceptance --tag demo --require-vm --json
rfb-cli forkd benchmark --tag demo --n 20 --json
rfb-cli forkd workload --tag demo --sandboxes 3 --rounds 2 --reuse-execs 25 --json
```

镜像内的自检（任选其一，全部在 guest 里执行。注意最小镜像的 `/bin` 只有
sh/bash/sleep/echo/true/false/netprobe/nproc(+解释器)，**没有 `cat`/`ls` 等
coreutils**——读文件用 `python3 -c "print(open('/etc/motd').read())"`）：

```bash
python3 -c "import demo; print(demo.VALUE)"      # 42
lua -e "print(require('demo').answer())"         # 42
/usr/local/bin/hello rfb                          # hello from rust-in-image, rfb!
python3 -c "print(open('/etc/motd').read())"     # RFB demo image (built from build.rfb)
```

## 进阶能力（SDK 层，两条路径通用）

- **fork**：`ZeroBootSandbox::fork(self)` → `(原沙箱延续, fork)`，checkpoint
  活动 VM 并恢复双子，继承全部 workspace/进程状态（§6b-4）
- **集群**：`rfb::cluster::ClusterProvider` 把 N 台 forkd controller 组成
  集群（最少在途调度/failover/熔断）（`sdk/BACKEND_SETUP.md` §7）
- **快照分片**：`RFB_ZBRT_SNAPSHOT_SHARDS=N` 让 100 并发热创建摊薄到
  ~54ms/个（§6b-2）
