# sdk/examples — 五语言示例总览

每个语言的 quickstart 都覆盖**同一场景、同一形状**（UNIFIED_API.md）：
wait_snapshot → create → ping → exec → write/read/ls → delete（forkd），
且都支持 `--backend forkd|zeroboot` 自由切换（zeroboot = 直连已运行的
ZBRT 桥，`RFB_ZBRT_TCP`，默认 `127.0.0.1:15000`——桥的拉起见下）。

| 语言 | 示例 | 前置 | 运行 |
|---|---|---|---|
| Python | `python/quickstart.py` | pip install rfb-sdk；forkd 栈或 ZBRT 桥 | `python3 quickstart.py --backend forkd sample` |
| Python | `python/host_quickstart.py` | KVM + root；`RFB_DEMO_ASSETS` 指向制品目录 | `sudo python3 host_quickstart.py` |
| Python | `python/repl.py` | 同 host_quickstart（宿主侧双后端 + 交互沙箱） | `sudo python3 repl.py --up` |
| Node.js | `nodejs/quickstart.mjs` | npm install rfb-sdk；forkd 栈或 ZBRT 桥 | `node quickstart.mjs --backend forkd sample` |
| Java | `java/`（独立 POM） | Maven Central 的 io.github.cricle:rfb-sdk | `mvn -q compile exec:java -Dexec.mainClass=example.Quickstart "-Dexec.args=--backend forkd sample"` |
| C# | `csharp/`（独立 csproj） | nuget.org 的 Rfb.Sdk；与控制器同侧网络 | `dotnet run -- --backend forkd sample` |
| Rust | `rust/`（独立 crate） | crates.io 的 rfb-sdk（features: forkd+zeroboot） | `cargo run -- --backend forkd sample` |

## 后端从哪来

- **forkd 栈**：`rfb-cli forkd backend-up --tag <tag> --tap forkd-tap0 ...`
  （拉控制器 + TAP + 快照，幂等），或用 `python/repl.py --backend forkd --up`。
- **ZBRT 桥**：`rfb-cli zeroboot up --tcp 127.0.0.1:15000`（VM + TCP 桥），
  或 `python/repl.py --up`（ZerobootHost 直驱，默认后端）。
- **Python 独有**：`rfb_sdk.host` 把后端编排做成库能力（无 CLI 依赖）——
  见 UNIFIED_API.md §10b。

## repl 示例的制品（不入 git）

`python/repl.py` 需要 `RFB_DEMO_ASSETS`（默认 `./assets/`，已 gitignore）：
firecracker、vmlinux、zeroboot-zbrt.ext4、forkd-agent.ext4、
forkd-controller、forkd（各支持 .gz）。一次性搭建：

```bash
bash sdk/examples/python/setup-demo-assets.sh
```

（内部：musl 静态构建 pid1 → `rfb-cli image build-rootfs` 产出两个 rootfs
→ 连同 resx 的 firecracker/kernel/forkd 二进制归位。）
