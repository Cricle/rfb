# RFB 统一 SDK 规范（v1 — 单一客户端）

目标：**四个语言（rust / python / csharp / java）只有一个公共客户端类型 `RfbClient`**。Rust 实现是基准（reference implementation），python/csharp/java SDK 是它的镜像移植，逐方法对照 Rust 签名。差异仅限语言习惯（Python snake_case 同步、C# PascalCase 异步 Task、Java camelCase 同步、Rust snake_case 异步 tokio）。

两条权威文件：`sdk/PROTOCOL.md` 管字节（wire contract）；本文件管 API 表面（public surface）。两者冲突时字节以 PROTOCOL.md 为准、表面以本文件为准。

---

## 1. 公共类型全集（不允许更多公共类型）

```
RfbClient          唯一客户端
Sandbox            沙箱门面（RfbClient 返回）
GuestStream        交互流（Sandbox.stream 返回；C# 名 Stream）
StreamEvent        流事件（kind: started|stdout|stderr|exit, data, code）
ExecResult         exec/eval 统一结果
DirEntry / GrepMatch / FileRead   文件操作结果
Snapshot / SandboxInfo            controller DTO
RfbError           错误基类 + Transport/HttpStatus/Decode/Remote/Validation 五子类
```

**三个协议实现（forkd controller HTTP、forkd guest NDJSON、ZBRT v1 帧）一律是内部模块，不进入公共 API**：

| 语言 | 内部化方式 |
|---|---|
| Rust | `rfb::client` 内部子模块 `pub(crate)`；复用 crate 内已有 `forkd::ForkdClient`/`ForkdGuestClient` 与 `protocol` 帧编解码 |
| Python | 模块名前缀下划线：`_forkd.py`、`_guest.py`、`_zbrt.py`；`__init__.py` 不导出它们 |
| C# | `internal` 类 |
| Java | `io.rfb.sdk.internal` 包，javadoc 标注 internal，公共导出不含 |

## 2. `RfbClient` 构造

| 语言 | 构造 |
|---|---|
| Python | `RfbClient(base_url=None, token=None, timeout_s=10.0)` |
| C# | `new RfbClient(string? baseUrl = null, string? token = null, double timeoutS = 10.0)` |
| Java | `new RfbClient()` / `new RfbClient(String baseUrl, String token, double timeoutS)` |
| Rust | `RfbClient::from_env() -> Result<Self, RfbError>` / `RfbClient::new(base_url, token: Option<String>, timeout: Duration)` |

`base_url=None` → 环境变量 `FORKD_URL`（默认 `http://127.0.0.1:8889`）；`token=None` → `FORKD_TOKEN`（非空才带 Bearer 头）。

## 3. `RfbClient` 方法（四语言一一对应；C# 首字母大写，Java 小驼峰，Rust 异步 `async fn` + `Result<_, RfbError>`）

| 方法 | 返回 | 说明 |
|---|---|---|
| `list_snapshots()` | `[Snapshot]` | GET /v1/snapshots |
| `snapshot(tag)` | `Snapshot?`（Rust `Option<Snapshot>`） | /info → 旧端点回退链，双 404 = null |
| `wait_snapshot(tag, timeout_s=60)` | `Snapshot` | 轮询 100ms；failed 立即抛 RemoteError；超时抛 |
| `create_sandbox(snapshot_tag, n=1, per_child_netns=false, memory_limit_mib=None, prewarm=false, live_fork=false, hugepages=false)` | `[Sandbox]` | Rust 用 `CreateOptions{..}` + `Default`；Java/C# 可选参数 |
| `list_sandboxes()` | `[Sandbox]` | |
| `connect(sandbox_or_id)` | `Sandbox` | 附着已存在沙箱（id 字符串或 Sandbox 均可） |
| `ping_sandbox(id)` | JSON 值（Rust `serde_json::Value`） | 原样返回 |
| `delete_sandbox(id)` | `void` | 2xx/404 均成功 |

## 4. `Sandbox` 方法

属性：`id`、`snapshot_tag`、`guest_addr`、`created_at_unix`、`info`。

构造/连接时的传输选项：Python/C#/Java `transport="ndjson"`（默认）`|"zbrt"`；Rust `GuestTransport::{Ndjson, Zbrt}`（默认 Ndjson）。**两种传输下以下方法名与返回形状完全一致**。

| 方法 | 返回 |
|---|---|
| `ping()` | `bool`（healthy） |
| `exec(args, cwd="/", timeout_s=60.0, stdin=b"")` | `ExecResult`（stdin 仅 ZBRT 投递；NDJSON 无 exec stdin 通道，非空 stdin 静默丢弃 — Rust 基线） |
| `eval(code, cwd=None, timeout_s=None)` | `ExecResult`（eval 的 output 映射为 stdout） |
| `ls(path=".")` | `[DirEntry(name, is_dir, size?)]` |
| `find(path=".", pattern)` | `[str]` |
| `grep(path=".", pattern)` | `[GrepMatch(path, line?, column?, text)]` |
| `read(path, offset=None, max_bytes=None)` | `FileRead(data: bytes, truncated, total_bytes?)` |
| `write(path, data, append=False, mode=None)` | `int`（bytes_written） |
| `stream(args, cwd=None, pty=None, env=None)` | `GuestStream` |
| `delete()` | `void` |

校验规则全部沿用 `PROTOCOL.md` §2.3（上限 4096/1024/1000/51200/1048576、路径/pattern/eval/cancel-id 规则），发送前本地执行，失败抛 `ValidationError`。

## 5. `GuestStream`

- `next_event()` → `StreamEvent(kind ∈ started|stdout|stderr|exit, data: bytes, code: int?)`；干净关闭返回 None（C# `IAsyncEnumerable<StreamEvent>` 结束即序列完成）。
- `send_input(text)` — 终结后调用抛 `RemoteError`。
- `stop()` — 幂等。

## 6. 结果对象字段（四语言一致）

- `ExecResult`：`exit_code:int`、`stdout:bytes`、`stderr:bytes`、`timed_out:bool` + 便捷 `stdout_text`/`stderr_text`（UTF-8）。
- `DirEntry`：`name`、`is_dir`、`size:int?`。 `GrepMatch`：`path`、`line?`、`column?`、`text`。 `FileRead`：`data`、`truncated`、`total_bytes?`。
- `Snapshot`：`tag, dir, created_at_unix, branched_from, pause_ms, diff_ms, diff_physical_bytes, diff_logical_bytes, warning, status, bootable, digest, provenance`（PROTOCOL.md §1.3 默认值）。
- `SandboxInfo`：`id, snapshot_tag, netns, created_at_unix, guest_addr, memory_limit_mib, pid, has_branched, branch_count`。

## 7. 错误体系（四语言一致；C# 异常类、Java unchecked 异常、Rust 枚举、Python 异常类）

```
RfbError
├─ TransportError      连接/读写失败、超时
├─ HttpStatusError     forkd 非 2xx（携带 status + message）
├─ DecodeError         响应/帧解码失败（含 strict codec 拒绝）
├─ RemoteError         对端 error（guest error 行、ZBRT Error 帧、forkd error 字段）
└─ ValidationError     本地校验失败（fail closed）
```

## 8. Rust 基准实现（`rfb::client`）

- 位置 `rfb/src/client/`；`lib.rs` 仅加一行 `pub mod client;`。
- 公共导出只有 §1 全集中的类型：`pub use client::{RfbClient, Sandbox, GuestStream, StreamEvent, StreamEventKind, ExecResult, DirEntry, GrepMatch, FileRead, Snapshot, SandboxInfo, RfbError, GuestTransport, CreateOptions};`
- 内部子模块 `pub(crate)`：controller HTTP 适配（复用 `rfb::forkd::ForkdClient`）、guest NDJSON 适配（复用 `rfb::forkd::ForkdGuestClient`/`ForkdGuestStream`）、ZBRT TCP 客户端（复用 `rfb::protocol` 帧编解码 + tokio TcpStream）。不新增依赖。
- 签名见 §2-§5 的 Rust 列；`RfbError` 枚举 `Transport(io::Error) / Http{status:u16, message:String} / Decode(String) / Remote(String) / Validation(String)`，实现 `std::error::Error + Display`，从 `ForkdClientError`/`ForkdGuestError`/`ContractError` 转换。
- 测试 `rfb/tests/client_facade.rs`：进程内 fake controller（TcpListener 手写极小 HTTP）+ fake guest NDJSON + fake ZBRT 帧服务；两条传输的 exec/eval/ls/read/write/ping/delete 形状一致性与错误映射。
- 门禁：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`，默认并行。

## 9. 验收清单（逐条核对）

1. **公共 API 中只有一个客户端类型 `RfbClient`**（四语言皆然）；协议实现均为内部（下划线模块 / `internal` / internal 包 / `pub(crate)`），公共导出不含 Forkd*、Zbrt* 等其他客户端类型。
2. §1 类型全集在四个语言全部存在且同名（仅大小写/惯用法差异）；错误五子类齐全。
3. §3/§4 方法四语言一一对应；参数名与默认值一致（Rust 用 `CreateOptions`/`GuestTransport` 表达可选参数）。
4. `Sandbox.exec/eval` 两种传输（NDJSON/ZBRT）结果字段一致（eval output → stdout）。
5. PROTOCOL.md §4 的 8 个黄金向量在**四个**实现/测试里编码、解码双向命中（Rust 放在内部模块的单测或集成测试里）。
6. 四个 README 均含「统一 API」章节 + 同一场景快速上手（建沙箱 → exec → read/write → delete），并声明 Rust 是基准实现、SDK 为镜像移植。
7. Python/C# 门面测试实测全绿；Rust 过完整门禁；Java 编译正确性静态审查通过。
