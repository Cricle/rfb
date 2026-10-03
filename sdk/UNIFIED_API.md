# sdk/UNIFIED_API.md — 四语言统一 API 契约（v1）

基准实现：Rust `rfb::client::RfbClient`（`rfb/src/client/facade.rs`）；Python / C# / Java 为逐方法
镜像移植。同一方法在四语言下**同名、同参、同结果形状、同 wire 行为**（wire 层见
[`PROTOCOL.md`](PROTOCOL.md)）；分歧以 `sdk/shared/` 的向量与说明为准修正。

## 1. 公共类型全集

公共 API 只有以下类型；协议适配类全部为 internal，不进入公共 API。

| 类别 | 类型 |
|---|---|
| 客户端 / 门面 | `RfbClient`、`Sandbox`、`GuestStream`（Rust 门面为避免与 `rfb::Sandbox` trait 撞名，结构体名为 `client::GuestSandbox`，语义即本表的 `Sandbox`） |
| 事件 | `StreamEvent`、`StreamEventKind`（`started` / `stdout` / `stderr` / `exit`） |
| 结果 DTO | `ExecResult`（Rust 门面同名 `client::GuestExecResult`）、`DirEntry`、`GrepMatch`、`FileRead`（§6） |
| controller DTO | `Snapshot`、`SandboxInfo` |
| 错误基类 + 5 子类 | 见 §7 |

各语言命名：Rust = `rfb::client::RfbClient` + `RfbError{…}`；Python = `rfb_sdk.*` + `*Error`；
Java = `io.rfb.sdk.*` + `*Error`（unchecked）；C# = `Rfb.Sdk.*` + `*Exception`。

## 2. RfbClient：构造与环境变量

`RfbClient(base_url=None, token=None, timeout_s=10.0)`（Rust 用 `RfbClient::from_env()` / 显式构造）：

| 参数 | 缺省解析 |
|---|---|
| `base_url` | env `FORKD_URL` → `http://127.0.0.1:8889`（未设**或空白**都回退默认）；仅接受 http/https 且带 host，否则本地 Validation |
| `token` | env `FORKD_TOKEN`；**非空才带** `Authorization: Bearer` 头 |
| `timeout_s` | 10 秒；必须 `> 0`（Java/C# 另拒绝 NaN/Inf），否则 Validation 错误 |

导出常量（四语言同名，rust 为 `rfb::client::DEFAULT_ZBRT_TCP` /
`GuestTransport` 枚举）：`TRANSPORT_NDJSON`="ndjson"、`TRANSPORT_ZBRT`="zbrt"、
`DEFAULT_ZBRT_TCP`="127.0.0.1:15000"——示例与调用方禁止内联这些字面量。

超时覆盖一次 HTTP 请求（连接 + 读）全程。线程安全：`RfbClient` 的 controller 请求共用一条
keep-alive 连接，Python 实现以内部锁把请求串行化（同一 client 并发调用是安全的，但不并行）；
guest 连接每请求新建。环境变量总表见 §10。guest 侧可选的 agent 认证
（`FORKD_AGENT_TOKEN`：配置后每条 guest 连接必须先发 auth 首帧）见 §10 与 `PROTOCOL.md §2.6`。

## 3. controller 生命周期（快照 / 沙箱）

| 方法 | 语义 |
|---|---|
| `list_snapshots()` | `GET /v1/snapshots` → `[Snapshot]` |
| `snapshot(tag)` | `/info` 端点 → 404 回退旧端点 → **双 404 = None** |
| `wait_snapshot(tag, timeout_s=60)` | 每 100ms 轮询 list；`status=ready` 且 `bootable=true` 才返回；`failed` → Remote 立即；超时 → **Transport**（超时属传输类错误） |
| `create_sandbox(tag, n=1, per_child_netns=False, memory_limit_mib=None, prewarm=False, live_fork=False, hugepages=False, transport="ndjson")` | 返回 `[Sandbox]`；transport ∈ {"ndjson","zbrt"}，其它值 Validation |
| `list_sandboxes()` | 存活池 → `[Sandbox]`（NDJSON 门面） |
| `connect(sandbox_or_id, transport=None)` | 传 `Sandbox` 原样附加（仅显式 transport 才覆盖）；传 id 则先 `list_sandboxes` 解析，未命中 → Remote（“sandbox not found”） |
| `ping_sandbox(id)` / `delete_sandbox(id)` | controller 原样 ping 值 / **2xx 与 404 都算删除成功** |

**统一温连接池（五语言一致，rfb-ben 的容量形态）**：ZBRT 与 NDJSON 下
所有 guest 操作（ping/fs/exec）都从每个 Sandbox 的温连接池借还——已 Hello 的空闲连接，
借出时空闲超 1s 才重发 Hello 验活（guest 不关空闲连接，热路径零额外
RTT）；一条连接可顺序跑多个 turn，同时只承载一个活跃 turn；并发 = 池中
多条连接各服务一个操作（池深 16）。复用连接上"请求未送达"的失败（写失败/
对端断开/EOF 截断）换新连接重试一次；读超时（请求可能已在 guest 执行）
与解码/guest 错误绝不重试。stream 交互会话保持独占连接。
（AF_VSOCK 直连不可用：Firecracker 的 vsock 设备**只以 UDS relay 实现**——
官方 spec "backed by a set of Unix Domain Sockets"，v1.16/v1.17 的 /vsock
都强制 uds_path，宿主内核没有 guest CID 的路由；`/dev/vhost-vsock` 是其它
VMM 的机制。uds:/ 与 TCP 中继是仅有的两条宿主路径。）

**NDJSON 池语义**（无握手可验活）：借出时空闲 >1s 的连接直接丢弃重拨
（不重发任何探测）；借出连接上的**任何失败都丢弃连接并原样抛出，不做
任何重试**（rust guest.rs 参考语义——exec 的双重执行不可接受，NDJSON 又
无握手可区分"未送达"与"已执行"）。**内存基准（同一 forkd 沙箱，ping+ls
×N，VmRSS）**：rust 3.2MB 恒定；python 22.7MB 恒定；java GC 后 120MB
低于基线（无泄漏）；c# 池化后 4000 ops +15.9MB 平台（修复前逐请求新连
+21MB）；node 修复监听器泄漏前 2000 ops 留驻 1.3GB（GC 不回落）——修复
后 2000→8000 ops 均稳定 +16MB。教训：node 的事件套接字复用必须
`removeAllListeners()` 再挂新监听，否则每次借出都累积旧闭包。

**池深 = 16（连接数扩展曲线实测）**：8 连接 16k ops/s → 16 连接
26k（+65%，= 裸帧地板 27269 的 96%）→ 32 平台（25.7k）→ 64 过度订阅
回落（8k）；16 = 拐点。并发度由调用方驱动（每线程借一条连接）。
**ZBRT/vsock 的天花板（实测闭环）**：scale 基准（每迭代 ping+ls 两个
RPC）32 并发 8.4k ops/s = **RPC 吞吐 ≈16.9k/s，已等于裸探针测到的 FC
vsock 设备天花板（14929）**——软件栈已 wire-bound，零代码余量。builtin
exec（echo）单连接 p50 ≈ ping（五 SDK 实测 1.0-1.3ms：内建命令在 guest
serve 内联直出，spawn_blocking 往返已消除）。与 NDJSON（26k，TAP 内核
数据面）的剩余差距是**设备层**差距：Firecracker 的 vsock 是用户态设备
仿真，包处理在 VMM 内串行化（并发下 RTT 线性劣化 0.63→2.2ms）；TAP 由
内核转发，VMM 零参与。这不是代码可修的——vsock 路径的选择依据是
无 TAP 网卡（零网络配置）而非吞吐。
**guest 运行时模型（三传输已收敛）**：NDJSON agent、ZBRT、RFB1 vsock
三种 guest serve 全部是**每连接独立 RuntimeService + workspace executor**
——turn 跨连接并行，单连接卡死不殃及后续连接；workspace 大小缓存按
workspace 根共享（进程级注册表），`max_workspace_bytes` 仍是全工作区
约束。ZBRT 连接上内建命令内联直出（panic 有 catch_unwind 边界，fail
-closed），真实进程 spawn 仍走阻塞工作线程（有序通道 + join 探测兜底
panic）；RFB1/ZBRT 共享同一帧读取任务纪律（`connection_util`）。
**UDS 直拨（性能路径）**：`guest_addr = "uds:<path>[@<guest_port>]"` 直拨
Firecracker 的 vsock relay UDS——无 TCP/中继跳，rtt 减半（0.6→0.32ms），
8 并发 fs ≈ 6.4k ops/srust 实测 6388 ops/s（idle 阈值优化后 11479 ops/s）；
**c# 亦原生支持**（`UnixDomainSocketEndPoint`，net8.0 target——netstandard2.1
抛明确错误；实测 2147 ops/s；NDJSON TCP 池化后 8579 ops/s）；python/node 同形态
2.2k/3.9k，受语言运行时
GIL/事件循环限制。java：JEP 380（Java 16+）原生有 UDS，但 SDK 基线是
release 8（需反射）且 AF_UNIX 的 SocketChannel **无 SO_TIMEOUT 等价物**
（挂死风险要自造看门狗）——验证过反射路径可行（relay OK），维持 TCP 中继
（0.6ms/RTT，8 并发 ~0.8-1.0k ops/s），除非未来把基线提到 16。

**guest→host（vsock 反向方向）**：guest 里连 vsock `(CID 2, port)`
——Firecracker 转发到宿主 `<uds>_<port>` 的 AF_UNIX 监听，**裸字节流**
（无 CONNECT 前导——那是 host→guest 方向的协议）。两件套：
- **guest 侧**：镜像烤入的 `/bin/vsockdial <port> [cid] [linger_ms]`
  applet（双向泵，stdin↔vsock↔stdout）。**FC 不支持半关闭**（写端
  shutdown = 整条转发连接终结），所以 stdin EOF 后泵安静退出、由
  `linger_ms`（缺省 1000）收尾：socket 空闲该毫秒数即优雅退出；
  `0` = 严格等 EOF（服务端会关连接的用法）。消息边界由载荷自带。
- **宿主侧**：python `ZerobootHost.expose(guest_port, target)` 把
  `<uds>_<guest_port>` 桥到宿主 TCP 目标（`unexpose`/`down()` 收掉）。
  逐端口显式暴露：guest 只能到达运营者点名的东西（no-egress 合同不破
  坏——通道只到宿主本机的 target，不通外网）。

**attach 句柄的 delete 语义**：rust/java/c# 的直连 `attach()`（已知
guest 地址、无控制器参与，VM 生命周期归桥的 runner 所有）返回的句柄是
**inert delete**——`delete()` 只清理本地连接池、**不向控制器发请求**
（对占位控制器发真删除会在本机恰有 forkd 时误删同 id 沙箱）。控制器
创建/列出的句柄 delete 照常走真删除。python/node 没有独立的 attach 入口
（直接构造与控制器创建同形），delete 行为不变，由调用方保证（quickstart
的 zeroboot 分支自行跳过 delete）。

**create 的独立预算**：五语言的 `create_sandbox` 对 POST /v1/sandboxes
使用 60s 的独立预算（快照恢复 restore+resume 实测可超 10s 基础超时——
超时会孤儿一个已落地的沙箱）；其余控制器请求保持客户端默认。

**清理兜底（异常情况由框架收尾）**：`Sandbox` 的删除在四个语言里都有
RAII 式入口——python `with sandbox:`（`__exit__` 调 `delete`）、java
`implements AutoCloseable`（try-with-resources）、C# `IAsyncDisposable`
（`await using`）、node `Symbol.asyncDispose`（`await using`，Node ≥ 20.11
才可用该语法，旧版保持 try/finally）。rust 无异步 Drop，保持显式
`delete()`（示例放在取值之后）。任何中途失败路径都不会泄漏活沙箱。

## 4. Sandbox：guest 操作（NDJSON 与 ZBRT 同名同结果形状）

| 方法 | 签名要点 | 行为要点 |
|---|---|---|
| `ping()` | → `bool` | NDJSON：仅 `pong==true` 算健康（`healthy` 字段不作数；应答另含附加键 `protocol_version`=1，客户端忽略未知键）；ZBRT：HealthAck 的 `healthy` |
| `exec(args, cwd="/workspace", timeout_s=60.0, stdin=b"")` | argv 非空，cwd/timeout 先本地校验 | NDJSON wire 无 stdin 通道，非空 stdin **本地 fail closed**（ValidationError，零帧：静默丢弃=命令无输入运行）；仅 ZBRT 送达；ZBRT 的 `argc` 是单字节，`>255` 本地 Validation（零帧，连 TCP 都不建）；单 turn stdout+stderr 聚合 `>16 MiB` → Remote；`exit_code` 缺失/非整数 → `-1`；旧 agent 的 `out`/`err` 键与 `stdout`/`stderr` 等价（并存时**当前键优先**）。**客户端读预算** = 基础超时 + exec 死线 + 5s margin（五语言一致）：长静默命令（如 `sleep 60`）不会先撞客户端超时——guest 自己的死线才是权威 |
| `eval(code, cwd=None, timeout_s=None)` | code 去空白非空、≤ 1 MiB；timeout > 0 | 输出映射 `stdout`（`output`，旧 `out` 等价），`stderr` 恒空，exit 取 `status`（旧 `exit_code` 等价，缺省 0）；**ZBRT 下本地 fail closed**（ValidationError，零帧上线：v1 无 eval opcode，见 `sdk/shared/README.md §1`） |
| `ls(path=".")` / `find(path, pattern)` / `grep(path, pattern)` | fs 路径 + pattern 校验 | `max_results=1000`（grep 另 `max_bytes=51200`）；find 返回 `[str]`，grep 返回 `[GrepMatch]` |
| `read(path, offset=None, max_bytes=None)` | `max_bytes` 必须 `1..=51200` | → `FileRead{data, truncated, total_bytes}` |
| `write(path, data, append=False, mode=None)` | 负载 ≤ 51200 字节 | → `bytes_written`（缺失 → Decode） |
| `stream(args, cwd=None, pty=None, env=None)` | argv 非空；cwd 校验 | **fail closed**：空 argv 两传输都拒绝；ZBRT 下 `pty=True` / 非空 env → Validation（“… is not supported over the ZBRT transport”），发送任何帧之前拒绝 |
| `delete()` | — | 同 `delete_sandbox(id)` |

`id` / `snapshot_tag` / `guest_addr` / `created_at_unix` / `info` / `transport` 为只读属性。

## 5. GuestStream：交互式流

| 方法 | 语义 |
|---|---|
| `next_event()` → `StreamEvent(kind, data, code)` 或 `None` | 干净关闭（终结 Exit 后或对端断开）→ `None`；NDJSON 事件映射见 `PROTOCOL.md §2.5`（旧 `out`/`err` 键照收）；ZBRT：**连接建立即发送 Hello 强制握手**（未握手帧被 guest 拒绝），ZBRT 无 started 帧，**首次 `next_event` 由客户端合成 `started`**（§9 场景 6），Output(0/1) → stdout/stderr，Exit → exit(code)，Error → Remote |
| `send_input(text)` | 发 `{"in": text}`；仅 NDJSON 支持（ZBRT v1 无输入通道 → Remote）；终结/停止后 → Remote |
| `stop()` | 幂等：NDJSON 发 `{"action":"stop"}`；ZBRT 发 Cancel（target=本请求 id）并等空 CancelAck，期间到的 Output 先缓冲 |

## 6. 结果 DTO

| DTO | 字段（缺省语义同 serde(default)：缺失不报错） |
|---|---|
| `ExecResult` | `exit_code:int`、`stdout:bytes`、`stderr:bytes`、`timed_out:bool`；`stdout_text`/`stderr_text`（UTF-8 replace 解码） |
| `DirEntry` | `name:str`、`is_dir:bool=false`、`size:int?` |
| `GrepMatch` | `path:str`、`line:int?`、`column:int?`、`text:str` |
| `FileRead` | `data:bytes`、`truncated:bool=false`、`total_bytes:int?` |
| `StreamEvent` | `kind`、`data:bytes`（exit 时为空）、`code:int?`（仅 exit） |

## 7. 错误分类（基类 + 5 子类）

| 类别 | 何时抛 | Rust | Python | Java | C# |
|---|---|---|---|---|---|
| Transport | 连接/读写失败、**一切超时**（controller 请求超时 `ForkdClientError::Timeout`、wait_snapshot 超时、ZBRT 读停顿、ZBRT 握手失败） | `RfbError::Transport` | `TransportError` | `TransportError` | `TransportException` |
| Http | forkd controller 返回非 2xx（携带 status + message） | `RfbError::Http` | `HttpStatusError` | `HttpStatusError` | `HttpStatusException` |
| Decode | 响应/帧无法解码，含严格 codec 拒绝（坏 magic/版本/flags/未知 kind/截断/尾随/超限）、缺结果字段 | `RfbError::Decode` | `DecodeError` | `DecodeError` | `DecodeException` |
| Remote | 对端报错：guest `error` 行、ZBRT Error 帧、controller body `error` 字段、controller 客户端分类为 Remote 的对端失败（`ForkdClientError::Remote`）、“sandbox not found”、快照 `failed`、对端提前关闭、ZBRT 单 turn 聚合输出 >16 MiB | `RfbError::Remote` | `RemoteError` | `RemoteError` | `RemoteException` |
| Validation | 发送前本地校验失败（§4/`PROTOCOL.md §2.3`）——非法 `base_url`/transport、空 argv、ZBRT argc>255、ZBRT pty/env、eval over ZBRT 等；**fail closed，零网络流量** | `RfbError::Validation` | `ValidationError` | `ValidationError` | `ValidationException` |

Python/Java/C# 均为基类单继承结构，按类别 catch 基类即可全覆盖。

## 8. 默认值总表

| 项 | 值 |
|---|---|
| controller URL / 端口 | env `FORKD_URL` → `http://127.0.0.1:8889`（未设或空白都回退默认） |
| controller 超时 | 10 s（每请求；超时属 Transport 类） |
| `wait_snapshot` 预算 / 轮询间隔 | 60 s / 100 ms |
| `exec` 超时 / cwd | 60 s / `/workspace`（四语言缺省一致；`/` 会被 agent 拒绝） |
| guest 端口 | NDJSON agent TCP **8888**；ZBRT vsock **5000**（→ TCP relay） |
| ZBRT 桥 TCP | env `RFB_ZBRT_TCP` → 常量 `DEFAULT_ZBRT_TCP` = `127.0.0.1:15000` |
| 工作区根 | `/workspace`（agent 侧可用 `RFB_AGENT_WORKSPACE` 覆盖） |
| 单帧 / 单行上限 | ZBRT payload ≤ **16 MiB**；NDJSON 行 ≤ **1 MiB** |
| ZBRT 单 turn 聚合输出 | stdout+stderr ≤ **16 MiB**，超限 → Remote |
| guest agent 认证 | `FORKD_AGENT_TOKEN` 非空才启用；首帧 `{"action":"auth","token":…}`，10 s 超时（`PROTOCOL.md §2.6`） |
| 路径 / pattern | 4096 / 1024 字节（UTF-8） |
| 结果数 / 结果与写入负载 | 1000 条 / 51200 字节（50 KiB） |
| eval 代码 | 1 MiB |
| exec timeout 上 wire | 整秒 ceil（min 1）；ZBRT ×1000，`None` → 0（无 deadline） |
| id（sandbox/cancel） | ≤ 128 字符 `[A-Za-z0-9_-]` |

## 9. 统一场景（§9 验收清单）

四语言同一套流程（`tests/` 各自的 facade 测试跑同一场景矩阵，NDJSON + ZBRT 双传输）：

1. `wait_snapshot(tag)` → 快照 ready 且 bootable；
2. `create_sandbox(tag)[0]` → exec `["echo","hello"]` → `exit_code=0`、`stdout="hello\n"`；
3. `write("notes.txt", b"hello")` → `read` 回读字节相等（append 可选）；
4. `ls / find / grep` 返回同形结果；
5. `eval("1+1")` → stdout 承载输出（NDJSON）；ZBRT 下本地 fail closed（Validation，零帧，见 §1）；
6. `stream`：started → stdout（send_input 回显）→ stop → exit（ZBRT 没有 started 帧，门面在首次 `next_event` 合成 `started`，见 §5）；
7. `delete()` → 2xx/404 均成功；
8. 全部校验失败路径（空 argv、逃逸路径、超限、非法 transport、ZBRT argc>255、ZBRT pty/env）在**发送前**抛 Validation。

## 10. 环境变量

| 变量 | 谁读 | 作用 |
|---|---|---|
| `FORKD_URL` | 四语言 `RfbClient` 缺省构造；`rfb-cli forkd *` | controller 地址（默认 `http://127.0.0.1:8889`；未设或空白都回退默认） |
| `FORKD_TOKEN` | 四语言 `RfbClient` 缺省构造 | controller Bearer token（非空才发头） |
| `FORKD_AGENT_TOKEN` | forkd agent（guest 侧，启用连接认证）；Python/Node.js/Java SDK guest 客户端（配置后每连接先发 auth 首帧）；Rust/C# 客户端尚未接入 | guest NDJSON 连接认证；非空 = 强制首帧 `{"action":"auth","token":…}`，10 s 超时（`PROTOCOL.md §2.6`） |
| `RFB_ZBRT_TCP` | 五语言 quickstart 的 `--backend zeroboot`；rfbsample | 已运行 ZBRT 桥的 TCP 地址（默认 `127.0.0.1:15000`） |
| `RFB_DEMO_ASSETS` | `examples/python/repl.py` | 宿主侧演示的制品目录（firecracker/vmlinux/zeroboot-zbrt.ext4） |
| `FORKD_KERNEL` / `FORKD_ROOTFS` / `FORKD_BIN` | `rfb-cli forkd snapshot-*` | 快照创建的内核 / rootfs / 官方 forkd 二进制路径覆盖 |
| `RFB_RUNTIME_BIN` | 部署流水线（约定注入点，仓库内代码不读） | 预编译静态 rfb-runtime 二进制路径 |
| `RFB_AGENT_WORKSPACE` | rfb-runtime agent | guest 工作区根覆盖（默认 `/workspace`，供宿主侧契约测试用） |

工具链变量的逐条说明见 [`BACKEND_SETUP.md §5`](BACKEND_SETUP.md)。

## 10b. 宿主侧编排（rfb_sdk.host — 目前 Python 独有）

Python SDK 额外携带**宿主侧编排**（`rfb_sdk/host.py`，纯标准库）——把"后端"
本身跑起来的能力，不经 rfb-cli。其余语言 SDK 保持纯客户端（rust 的对应物是
`rfb-cli`）；此条列入分歧清单 §11。

- **`ZerobootHost`**：直驱 Firecracker（UDS 管理 API，逐步有界等待，
  `PR_SET_PDEATHSIG` 防孤儿）+ vsock 中继 UDS→TCP 桥（`CONNECT <port>\n` /
  `OK ...\n` 前导协议，与 Rust 侧 `vsock_relay.rs` 单源对齐）；
  `sandbox()` 返回与 forkd 完全同形状的 `Sandbox` 门面（transport="zbrt"）。
- **`ForkdHost`**：直拉 forkd-controller、`ip(8)` 幂等收敛 TAP、官方 forkd
  二进制建快照、共享 TAP 的残留沙箱收敛；`client()` 返回 `RfbClient`。
- **统一生命周期**：`alive() / up()（幂等：健康复用、残留按 run 目录 scoped
  清理）/ down() / client()/sandbox()`。
- **清理纪律**：SIGTERM 优先（父进程自己收尸），SIGKILL 只对幸存者——
  SIGKILL 的孤儿 VM 会变不可收尸僵尸，控制器的共享 TAP 门把僵尸也算活
  沙箱；ForkdHost 进程为 `PR_SET_CHILD_SUBREAPER` + waitpid 收尸线程；
  `forkd snapshot` 留下的 parent VM（live-fork 模板）在返回前主动退役并等
  进程表排空。
- **`restore_asset(assets_dir, name, target)`**：制品目录解析（`.gz` 自动
  解包、chmod 755、已存在的目标不覆盖）。

## 11. 已知的有意分歧（documented divergences）

- **guest shell 能力边界（busybox sh v1，agent 实战 2026-10 确认）**：
  guest 镜像的 `/bin/sh` 是自研多调用二进制（`rfb-busybox`，模块头即规格）：
  支持 `;`/换行、`&&`/`||`、管道、重定向（`>`/`>>`/`<`/`2>&1` 等）、引号、
  `$?`、内建 `exit/echo/cd/pwd/true/false/:`；**不支持**变量展开（除 `$?`）、
  循环、命令替换、glob，且 **只接受 `sh -c SCRIPT`**（`sh SCRIPT` 与交互
  模式报 "interactive shell is not supported"）。agent 工具调用可用 -c
  形式或脚本文件（**sh FILE 与 shebang 脚本均支持**——复杂任务电池 X1
  的教训：此前 shebang 脚本在 guest 里永远跑不起来，kernel shebang 机制
  调 `/bin/sh <文件>` 而 sh 只认 -c；已支持文件模式）。镜像内的 applet：
  `sh/bash/sleep/echo/true/false/netprobe/vsockdial/nproc`——没有
  rm/cat/ls/tail/wc（ls/cat 走 FS RPC），脚本里 `rm` 要换成 `: > file`
  截断、读文件走 read RPC。覆写文件**保留原权限**（write_atomic 原子
  替换曾把 chmod +x 的脚本在下次编辑后打回 0644——已修复）。

- **ZBRT 的 exec deadline 命中**：NDJSON 返回 `ExecResult(timed_out=True,
  exit_code=-1)`；ZBRT 走 guest `Error` 帧（"command timed out"）→ 五 SDK
  一致抛 Remote/guest 错误——ZBRT v1 没有 timed-out 线标志，按消息文本
  反推 timed_out 会分叉传输语义（rust 基线 `client/zbrt.rs` P2 决定，
  实战 2026-10 验证五语言行为一致）。deadline 的**截断本身**两种传输都
  真实生效（子进程被杀）。

- **`rfb-adk`（原 rfb-rig）**：ADK（adk-rust 2.2，edition 2024 / MSRV 1.95）
  工具面 + `sandbox_agent` 装配（真 runner + 七工具 + OpenAI 兼容网关）；
  模型可见错误做红线（transport/execution 诊断只进 operator 侧 metadata）。
  为自治 agent 长驻场景的关键修复：**FC fork 必须在专职 PDEATHSIG holder
  线程上**——tokio 阻塞池线程空闲 ~10s 即回收，从它 fork 的 FC 会在 boot
  后 ~10s 整被 SIGKILL（真机演练实测：所有 guest 连接 t≈10s 集体死亡、
  FC 无退出日志）；holder 线程 spawn 后停驻到 VM teardown（Drop 先杀子
  进程再放线程）。

- **旧键回退顺序**：~~C# 优先旧键~~（已过时）——实测四语言（Python/Node/Java/C#）与 Rust 基准
  一致，都是**当前键优先**（`stdout`/`output`，见 `GuestResults.cs` 的
  `Prop(v, "stdout") ?? Prop(v, "out")`）。分歧清单保留此条以记录勘误。
- **exec 缺省 cwd**：~~Python/C# 为 `"/"`~~（已过时）——实测四语言的缺省 cwd 都是
  `/workspace`（代码为准；agent 拒绝 `/` 作为 cwd）。建议显式传 cwd。
- **stream `done` 事件**：C#/Java 把 `{"done":true}` 映射为无码 Exit；Python 目前忽略该行
  （依赖其 `done` 终结键语义在请求路径终止）。
- **stream 信号终结行**：guest 对被信号杀死的子进程发出 `{"exit_code":null}`（无 `done`）。
  Python/Node/Rust 视为 Exit(null) 终结；Java/C# 历史上抛 Decode——已统一为
  “键存在即终结，null 值 → Exit(null)”。
- **agent 认证接入**：`FORKD_AGENT_TOKEN` 的 agent 侧门禁已强制（`PROTOCOL.md §2.6`）；客户端侧
  Python/Node.js/Java 已发送 auth 首帧，当前 Rust/C# 尚未接入——agent 配置了 token 时这些语言的
  guest 连接会被 `{"error":"authentication required"}` 拒绝（§10）。
