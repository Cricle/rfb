# sdk/examples — 五语言示例总览

每个语言的 quickstart 都是同一件事：`--backend forkd|zeroboot` 二选一接入，
然后 **ping → exec → write → read → ls** 五个直接调用（UNIFIED_API.md 的
统一形状，两后端零差别），forkd 侧失败也会 delete 不泄漏沙箱。

| 语言 | 目录 | 前置 | 运行 |
|---|---|---|---|
| Python | `python/` | `pip install rfb-sdk`；forkd 栈或 ZBRT 桥 | `python3 quickstart.py --backend forkd sample` |
| Node.js | `nodejs/` | `npm install rfb-sdk`；同上 | `node quickstart.mjs --backend forkd sample` |
| Java | `java/`（独立 POM） | Maven Central 的 io.github.cricle:rfb-sdk | `mvn -q compile exec:java -Dexec.mainClass=example.Quickstart "-Dexec.args=--backend forkd sample"` |
| C# | `csharp/`（独立 csproj） | nuget.org 的 Rfb.Sdk；与控制器同侧网络 | `dotnet run -- --backend forkd sample` |
| Rust | `rust/`（独立 crate） | crates.io 的 rfb-sdk（features: forkd+zeroboot） | `cargo run -- --backend forkd sample` |

## 后端从哪来

- **forkd 栈**（控制器默认 `FORKD_URL`=`http://127.0.0.1:8889`）：
  `rfb-cli forkd backend-up --tag <tag> --tap forkd-tap0 ...`（拉控制器 +
  TAP + 快照，幂等），或 `python/repl.py --backend forkd --up`。
- **ZBRT 桥**（`RFB_ZBRT_TCP`，默认 `127.0.0.1:15000`）：
  `rfb-cli zeroboot up --tcp 127.0.0.1:15000`（VM + TCP 桥），
  或 `python/repl.py --up`（ZerobootHost 直驱，默认后端）。
- **Python 独有**：`rfb_sdk.host` 把后端编排做成库能力（无 CLI 依赖）——
  见 UNIFIED_API.md §10b。

## python/ 目录的其它内容

- `repl.py` — 宿主侧编排 + 交互式沙箱演示（`rfb_sdk.host` 双后端自举，
  KVM + root）：`sudo python3 repl.py --up`（`--down` 停后端）；
  制品一次性搭建 `bash setup-demo-assets.sh`（不入 git，默认 `./assets/`）。
