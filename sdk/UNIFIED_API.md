# sdk/UNIFIED_API.md — 四语言统一 API 契约（v1）

基准实现：Rust `rfb::client::RfbClient`（`rfb/src/client/facade.rs`）；Python / C# / Java 为逐方法
镜像移植。同一方法在四语言下**同名、同参、同结果形状、同 wire 行为**（wire 层见
[`PROTOCOL.md`](PROTOCOL.md)）；分歧以 `sdk/shared/` 的向量与说明为准修正。

## 1. 公共类型全集

公共 API 只有以下类型；协议适配类全部为 internal，不进入公共 API。

| 类别 | 类型 |
|---|---|
| 客户端 / 门面 | `RfbClient`、`Sandbox`、`GuestStream` |
| 事件 | `StreamEvent`、`StreamEventKind`（`started` / `stdout` / `stderr` / `exit`） |
| 结果 DTO | `ExecResult`、`DirEntry`、`GrepMatch`、`FileRead`（§6） |
| controller DTO | `Snapshot`、`SandboxInfo` |
| 错误基类 + 5 子类 | 见 §7 |

各语言命名：Rust = `rfb::client::RfbClient` + `RfbError{…}`；Python = `rfb_sdk.*` + `*Error`；
Java = `io.rfb.sdk.*` + `*Error`（unchecked）；C# = `Rfb.Sdk.*` + `*Exception`。

## 2. RfbClient：构造与环境变量

`RfbClient(base_url=None, token=None, timeout_s=10.0)`（Rust 用 `RfbClient::from_env()` / 显式构造）：

| 参数 | 缺省解析 |
|---|---|
| `base_url` | env `FORKD_URL` → `http://127.0.0.1:8889`；仅接受 http/https 且带 host |
| `token` | env `FORKD_TOKEN`；**非空才带** `Authorization: Bearer` 头 |
| `timeout_s` | 10 秒；必须 `> 0`（Java/C# 另拒绝 NaN/Inf），否则 Validation 错误 |

超时覆盖一次 HTTP 请求（连接 + 读）全程。环境变量总表见 §10。

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

## 4. Sandbox：guest 操作（NDJSON 与 ZBRT 同名同结果形状）

| 方法 | 签名要点 | 行为要点 |
|---|---|---|
| `ping()` | → `bool` | NDJSON：仅 `pong==true` 算健康（`healthy` 字段不作数）；ZBRT：HealthAck 的 `healthy` |
| `exec(args, cwd="/", timeout_s=60.0, stdin=b"")` | argv 非空，cwd/timeout 先本地校验 | NDJSON wire 无 stdin 通道，非空 stdin **静默丢弃**（Rust 基线；仅 ZBRT 送达）；`exit_code` 缺失/非整数 → `-1`；旧 agent 的 `out`/`err` 键与 `stdout`/`stderr` 等价（并存时优先后者） |
| `eval(code, cwd=None, timeout_s=None)` | code 去空白非空、≤ 1 MiB；timeout > 0 | 输出映射 `stdout`（`output`，旧 `out` 等价），`stderr` 恒空，exit 取 `status` 缺省 0；ZBRT 无 eval opcode → 一轮 Execute `argv=["eval", code]`、空 stdin（`sdk/shared/README.md §1`） |
| `ls(path=".")` / `find(path, pattern)` / `grep(path, pattern)` | fs 路径 + pattern 校验 | `max_results=1000`（grep 另 `max_bytes=51200`）；find 返回 `[str]`，grep 返回 `[GrepMatch]` |
| `read(path, offset=None, max_bytes=None)` | `max_bytes` 必须 `1..=51200` | → `FileRead{data, truncated, total_bytes}` |
| `write(path, data, append=False, mode=None)` | 负载 ≤ 51200 字节 | → `bytes_written`（缺失 → Decode） |
| `stream(args, cwd=None, pty=None, env=None)` | argv 非空；cwd 校验 | **fail closed**：空 argv 两传输都拒绝；ZBRT 下 `pty=True` / 非空 env → Validation（“… is not supported over the ZBRT transport”），发送任何帧之前拒绝 |
| `delete()` | — | 同 `delete_sandbox(id)` |

`id` / `snapshot_tag` / `guest_addr` / `created_at_unix` / `info` / `transport` 为只读属性。

## 5. GuestStream：交互式流

| 方法 | 语义 |
|---|---|
| `next_event()` → `StreamEvent(kind, data, code)` 或 `None` | 干净关闭（终结 Exit 后或对端断开）→ `None`；NDJSON 事件映射见 `PROTOCOL.md §2.5`（旧 `out`/`err` 键照收）；ZBRT：Output(0/1) → stdout/stderr，Exit → exit(code)，Error → Remote |
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
| Transport | 连接/读写失败、**一切超时**（请求、wait_snapshot、ZBRT 读停顿） | `RfbError::Transport` | `TransportError` | `TransportError` | `TransportException` |
| Http | forkd controller 返回非 2xx（携带 status + message） | `RfbError::Http` | `HttpStatusError` | `HttpStatusError` | `HttpStatusException` |
| Decode | 响应/帧无法解码，含严格 codec 拒绝（坏 magic/版本/flags/未知 kind/截断/尾随/超限）、缺结果字段 | `RfbError::Decode` | `DecodeError` | `DecodeError` | `DecodeException` |
| Remote | 对端报错：guest `error` 行、ZBRT Error 帧、controller body `error` 字段、“sandbox not found”、快照 `failed`、对端提前关闭 | `RfbError::Remote` | `RemoteError` | `RemoteError` | `RemoteException` |
| Validation | 发送前本地校验失败（§4/`PROTOCOL.md §2.3`）——**fail closed，零网络流量** | `RfbError::Validation` | `ValidationError` | `ValidationError` | `ValidationException` |

Python/Java/C# 均为基类单继承结构，按类别 catch 基类即可全覆盖。

## 8. 默认值总表

| 项 | 值 |
|---|---|
| controller URL / 端口 | env `FORKD_URL` → `http://127.0.0.1:8889` |
| controller 超时 | 10 s（每请求） |
| `wait_snapshot` 预算 / 轮询间隔 | 60 s / 100 ms |
| `exec` 超时 / cwd | 60 s / `"/"`（Java NDJSON 缺省 cwd 为 `/workspace`，见 §11） |
| guest 端口 | NDJSON agent TCP **8888**；ZBRT vsock **5000**（→ TCP relay） |
| 工作区根 | `/workspace`（agent 侧可用 `RFB_AGENT_WORKSPACE` 覆盖） |
| 单帧 / 单行上限 | ZBRT payload ≤ **16 MiB**；NDJSON 行 ≤ **1 MiB** |
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
5. `eval("1+1")` → stdout 承载输出；ZBRT 下帧字节命中共享向量；
6. `stream`：started → stdout（send_input 回显）→ stop → exit；
7. `delete()` → 2xx/404 均成功；
8. 全部校验失败路径（空 argv、逃逸路径、超限、非法 transport、ZBRT pty/env）在**发送前**抛 Validation。

## 10. 环境变量

| 变量 | 谁读 | 作用 |
|---|---|---|
| `FORKD_URL` | 四语言 `RfbClient` 缺省构造；`rfb-cli forkd *` | controller 地址（默认 `http://127.0.0.1:8889`） |
| `FORKD_TOKEN` | 四语言 `RfbClient` 缺省构造 | Bearer token（非空才发头） |
| `FORKD_KERNEL` / `FORKD_ROOTFS` / `FORKD_BIN` | `rfb-cli forkd snapshot-*` | 快照创建的内核 / rootfs / 官方 forkd 二进制路径覆盖 |
| `RFB_RUNTIME_BIN` | 部署流水线（约定注入点，仓库内代码不读） | 预编译静态 rfb-runtime 二进制路径 |
| `RFB_AGENT_WORKSPACE` | rfb-runtime agent | guest 工作区根覆盖（默认 `/workspace`，供宿主侧契约测试用） |

工具链变量的逐条说明见 [`BACKEND_SETUP.md §5`](BACKEND_SETUP.md)。

## 11. 已知的有意分歧（documented divergences）

- **旧键回退顺序**：响应同时含新旧键时，Java/Python 优先当前键（`stdout`/`output`），C# 优先旧键
  （`out`）；两者只在“同一响应同时携带两套不同值”这种病态场景下不同。
- **exec 缺省 cwd**：Python/C# 为 `"/"`；Java 的 `exec(args, null, …)` 在 NDJSON 上 wire 为
  `/workspace`（agent 会拒绝 `/` 作为 cwd）。建议显式传 cwd。
- **stream `done` 事件**：C#/Java 把 `{"done":true}` 映射为无码 Exit；Python 目前忽略该行
  （依赖其 `done` 终结键语义在请求路径终止）。
