# RFB 外部 SDK 协议规范（Java / C# / Python 共享）

本文件是 `sdk/` 下三个语言 SDK 的唯一 wire 协议依据。规范来自仓库 Rust 实现（权威源文件见每节末尾）。SDK 不得发明新的 wire 格式；所有帧、字段、限制、校验规则必须与本规范一致。

---

## 1. forkd controller（HTTP/JSON REST）

权威源：`rfb/src/forkd/controller.rs`

### 1.1 连接

- 基地址：环境变量 `FORKD_URL`，默认 `http://127.0.0.1:8889`。
- 认证：环境变量 `FORKD_TOKEN`（非空时，所有请求带 `Authorization: Bearer <token>`）。
- 默认超时 10 秒。
- 非 2xx 响应 → 错误：优先解析响应体 JSON 的 `error` 字符串字段；否则取响应体前 1024 字符。
- 沙箱 id 校验（用于 ping/delete 路径参数）：非空、长度 ≤ 128、仅 `[A-Za-z0-9_-]`。

### 1.2 端点

| 方法 | 路径 | 请求体 | 响应 |
|---|---|---|---|
| GET | `/v1/snapshots` | — | `SnapshotInfo[]` |
| GET | `/v1/snapshots/{tag}/info` | — | `SnapshotInfo`；404 时回退 `GET /v1/snapshots/{tag}`；两个都 404 → `null`/`None` |
| POST | `/v1/sandboxes` | `CreateSandboxRequest` | `SandboxInfo[]` |
| GET | `/v1/sandboxes` | — | `SandboxInfo[]` |
| POST | `/v1/sandboxes/{id}/ping` | — | 任意 JSON 值 |
| DELETE | `/v1/sandboxes/{id}` | — | 2xx 或 404 均视为成功 |

### 1.3 DTO（JSON 字段名与类型）

`CreateSandboxRequest`（请求体，全部必填，nullable 用 null）：

```json
{
  "snapshot_tag": "string",
  "n": 1,
  "per_child_netns": false,
  "memory_limit_mib": null,
  "prewarm": false,
  "live_fork": false,
  "hugepages": false
}
```

`SnapshotInfo`（`serde(default)` 字段缺失时用默认值）：

```json
{
  "tag": "string",
  "dir": "",
  "created_at_unix": null,
  "branched_from": null,
  "pause_ms": null,
  "diff_ms": null,
  "diff_physical_bytes": null,
  "diff_logical_bytes": null,
  "warning": null,
  "status": "",
  "bootable": false,
  "digest": null,
  "provenance": null
}
```

- 整数时间戳/大小为 i64/u64（null 允许）；`provenance` 保持原样（任意 JSON 值）。

`SandboxInfo`：

```json
{
  "id": "string",
  "snapshot_tag": "string",
  "netns": null,
  "created_at_unix": null,
  "guest_addr": "",
  "memory_limit_mib": null,
  "pid": null,
  "has_branched": false,
  "branch_count": 0
}
```

### 1.4 便捷语义

- `snapshot_ready(tag)`：`list_snapshots` 中存在 `tag` 相同且 `status` 忽略大小写等于 `"ready"` 且 `bootable == true` 的快照。
- `wait_for_snapshot_ready(tag, timeout)`：轮询 `list_snapshots`，间隔 100ms；`status == "failed"`（忽略大小写）→ 立即报错；超时未 ready → 报错。

---

## 2. forkd guest（TCP + 换行分隔 JSON）

权威源：`rfb/src/forkd/guest.rs`、`rfb/src/guest/*.rs`

### 2.1 传输

- TCP 连接到沙箱的 `guest_addr`（controller 返回）。
- 每个请求一行 JSON（`\n` 结尾）；响应为一行或多行 JSON，读到**终结行**为止。
- 终结行判定：JSON 对象含以下任一键：`exit_code`、`pong`、`results`、`entries`、`matches`、`data`、`content`、`output`、`status`、`ok`、`healthy`、`done`、`cancelled`、`bytes_written`。
- 任一响应行含字符串键 `"error"` → 立即抛远程错误。
- 单行上限 `MAX_LINE_BYTES = 1 MiB`（1048576 字节），超限或未以 `\n` 结尾即报错。
- 默认超时 10 秒（连接/读/写）。

### 2.2 动作

| 动作 | 请求 JSON | 终结响应 |
|---|---|---|
| ping | `{"action":"ping"}` | 含 `pong` 的行 |
| exec | `{"action":"exec","cwd":"<guest路径>","args":["cmd","arg"],"timeout":<秒>}` | 含 `exit_code` 的行 |
| eval | `{"action":"eval","cwd":"<guest路径>","code":"..."}`（可选 `"timeout":<秒>`，RFB 毫秒向上取整为秒，最少 1） | 含 `output`/`status` 等的行 |
| ls | `{"action":"ls","path":".","max_results":1000}` | 含 `entries` 的行 |
| find | `{"action":"find","path":".","pattern":"...","max_results":1000}` | 含 `matches` 的行 |
| grep | `{"action":"grep","path":".","pattern":"...","max_results":1000,"max_bytes":51200}` | 含 `matches` 的行 |
| read | `{"action":"read","path":"...","offset":null,"max_bytes":null}`（可选键，null 可省略） | 含 `data` 的行 |
| write | `{"action":"write","path":"...","data":[bytes],"append":false,"mode":null}` | 含 `bytes_written` 的行 |
| stream | `{"action":"stream","args":["cmd"],"cwd":"...","pty":true,"env":{...}}`（可选键） | 会话开始，后续为事件行 |

- `write.data` 是字节数组（JSON number 数组）。
- stream 会话：同一 TCP 连接上继续发送 `{"in":"文本"}`（写入 stdin）或 `{"action":"stop"}`（请求终止）；持续读事件行直到出现 `exit_code`（终结）。stop/终结后再发送输入应报错。

### 2.3 限制与校验（发送前本地校验，fail closed）

常量（`rfb/src/guest/limits.rs`）：

| 常量 | 值 |
|---|---|
| `MAX_GUEST_PATH_BYTES` | 4096 |
| `MAX_GUEST_PATTERN_BYTES` | 1024 |
| `MAX_GUEST_RESULTS` | 1000 |
| `MAX_GUEST_RESULT_BYTES` | 51200（50 KiB） |
| `MAX_GUEST_CODE_BYTES` | 1048576（1 MiB） |

规则：

- 结构化 fs 路径（ls/find/grep 的 `path`）：非空、≤4096 字节、无 NUL、不允许 `\`（含出现于任意位置）、允许 `/workspace` 前缀的绝对路径，其余 `/` 开头拒绝、任何路径段不得为 `..`。默认值 `"."`。
- 文件路径（read/write 与 eval/stream 的 cwd）：非空、≤4096 字节、无 NUL、无 `\`、无 `X:` 盘符前缀、任何段不得为 `..`（允许绝对或相对）。
- pattern（find/grep）：非空、无 NUL、≤1024 字节。
- 数量上限（max_results / max_bytes / 代码长度）：> 0 且 ≤ 对应上限（0 拒绝）。
- eval：`code` 去空白后非空、≤1 MiB；timeout 不得为 0。
- cancel id（可选）：非空、≤128、仅 `[A-Za-z0-9_-]`。

### 2.4 结果类型（JSON）

- `ls` → `{"entries":[{"name":"...","is_dir":false,"size":null}],"truncated":false}`（`size` 可省略）
- `find` → `{"matches":["path",...],"truncated":false}`
- `grep` → `{"matches":[{"path":"...","line":1,"column":1,"text":"..."}],"truncated":false}`（`line`/`column` 可省略）
- `read` → `{"data":[bytes],"truncated":false,"total_bytes":null}`（`total_bytes` 可省略）
- `write` → `{"bytes_written":123}`
- `eval` → `{"output":[bytes],"status":0,"timed_out":false}`（`status` 可省略）
- cancel → `{"cancelled":true}`
- ping → `{"pong":...}` / health → `{"healthy":true,...}`

---

## 3. ZBRT v1（二进制帧协议，ZeroBoot）

权威源：`rfb-runtime/src/zeroboot_protocol.rs`、`rfb-runtime/src/zeroboot_connection.rs`、`rfb-runtime/src/workspace_executor/cancel.rs`

### 3.1 帧

```
偏移  大小  字段
0     4    magic "ZBRT"
4     1    version = 1
5     1    kind
6     2    flags（u16 BE，必须为 0，非 0 拒绝）
8     16   request_id（128-bit，原样字节）
24    4    payload_len（u32 BE，≤ 16 MiB = 16777216）
28    n    payload
```

- 帧头 28 字节。解码必须拒绝：错误 magic/version、flags≠0、未知 kind、payload 超限、截断。
- payload 内部字符串/字节字段均为 **u32 BE 长度前缀**。所有 payload 解码必须拒绝截断**和尾部多余字节**（strict codec）。

### 3.2 kind 表

| 值 | 名称 | 方向 | payload |
|---|---|---|---|
| 1 | Hello | C→S | client 字符串 + count(u8) + capability 字符串 × count |
| 2 | HelloAck | S→C | server 字符串 + count(u8) + capability 字符串 × count |
| 3 | Execute | C→S | argc(u8) + argv × argc + cwd 标志(u8)+cwd + stdin(u32 前缀) + timeout_ms(u32 BE) |
| 4 | Output | S→C | stream(u8) + data(u32 前缀)。stream：0=stdout，1=stderr |
| 5 | Exit | S→C | code(i32 BE) + signal 标志(u8) + [signal(u32 BE)] |
| 6 | Cancel | C→S | reason 标志(u8)+[reason] + target 标志(u8)+[16 字节 target id]；**legacy**：reason 后直接结束（无 target 字节）→ target=None |
| 7 | CancelAck | S→C | 空 payload |
| 8 | Fs | C→S | op(u8) + path 字符串 + data(u32 前缀，JSON 参数) |
| 9 | FsResult | S→C | JSON 结果（对应 2.4 节各 fs 结果结构） |
| 10 | Health | C→S | healthy 标志(u8) + message 标志(u8)+[message] |
| 11 | HealthAck | S→C | Health 编码（同 Health payload） |
| 12 | Error | S→C | code(u32 BE) + message 字符串 |
| 13 | Result | S→C | 保留 |

- Execute 的 cwd 标志：1 = 后跟字符串；0 = 无 cwd。
- 能力表 `ZBRT_V1_CAPABILITIES = ["execute","stream","deadline","health","cancel","filesystem"]`。
- guest 服务端名（HelloAck.server）：`"rfb-zeroboot-guest"`。

### 3.3 Fs opcode

`data` 为 JSON 参数对象（见 2.4 节请求结构，去掉 `action` 键；帧内 `path` 字段为准）：

| op | 操作 | data JSON |
|---|---|---|
| 1 | ls | `{"max_results":1000}` |
| 2 | find | `{"pattern":"...","max_results":1000}` |
| 3 | grep | `{"pattern":"...","max_results":1000,"max_bytes":51200}` |
| 4 | read | `{"offset":null,"max_bytes":null}` |
| 5 | write | `{"data":[bytes],"append":false,"mode":null}` |

路径归一化（guest 侧）：`/workspace` → `.`；`/workspace/x` → `x`；其他透传（由 PathPolicy 拒绝）。

### 3.4 会话语义（zeroboot_connection.rs）

- Hello 可选（服务端对未 Hello 的连接自动就绪）；Hello 后服务端返回 HelloAck。
- **单活跃请求**：一条连接同一时刻最多一个 Execute；第二个 Execute → `Error("a turn is already active")`；重复 request_id → `Error("duplicate request id")`；空 argv → `Error("argv is empty")`。
- Execute 响应流：0..n 个 `Output` 帧（严格先于终结帧）→ 恰好一个终结帧：`Exit`（完成/取消；取消 code=-1）或 `Error`（运行时错误）。
- Cancel：幂等。目标已终结或无活跃请求 → `CancelAck`（空 payload）；target 与活跃请求不符 → `Error`。
- Health → `HealthAck`（healthy=true, message="ready"）。
- 未知 kind → fail closed：`Error(code=1)`。
- request_id 客户端生成（128-bit 随机或计数器），服务端原样回显。

---

## 4. 黄金测试向量（三个 SDK 的 codec 测试必须全部命中）

request_id 统一用 `000102030405060708090a0b0c0d0e0f`。

| 名称 | 帧 hex |
|---|---|
| HELLO（client="sdk-test", caps=["execute","stream"]） | `5a42525401010000000102030405060708090a0b0c0d0e0f000000220000000873646b2d746573740200000007657865637574650000000673747265616d` |
| HELLOACK（server="rfb-zeroboot-guest", 6 caps） | `5a42525401020000000102030405060708090a0b0c0d0e0f0000005a000000127266622d7a65726f626f6f742d67756573740600000007657865637574650000000673747265616d00000008646561646c696e65000000066865616c74680000000663616e63656c0000000a66696c6573797374656d` |
| EXECUTE（argv=["echo","hi"], cwd="/workspace", stdin="abc", timeout_ms=1500） | `5a42525401030000000102030405060708090a0b0c0d0e0f0000002902000000046563686f000000026869010000000a2f776f726b737061636500000003616263000005dc` |
| OUTPUT（stream=1, data="err line\n"） | `5a42525401040000000102030405060708090a0b0c0d0e0f0000000e0100000009657272206c696e650a` |
| EXIT（code=0, no signal） | `5a42525401050000000102030405060708090a0b0c0d0e0f000000050000000000` |
| CANCEL（reason="user", no target） | `5a42525401060000000102030405060708090a0b0c0d0e0f0000000a01000000047573657200` |
| CANCEL_LEGACY（reason="user"，无 target 字节） | `5a42525401060000000102030405060708090a0b0c0d0e0f00000009010000000475736572` |
| ERROR（code=1, message="argv is empty"） | `5a425254010c0000000102030405060708090a0b0c0d0e0f00000015000000010000000d6172677620697320656d707479` |

---

## 5. SDK 公共设计约束

- **对用户简单**：所有客户端开箱即用（默认值取自环境变量/协议默认），方法参数直接用语言原生类型（字符串、列表、字节数组、整数），不强迫用户构造请求对象；类型化 DTO 仅作为可选项。
- 不修改 `sdk/` 之外的任何现有文件；各 SDK 自带 README.md 与 tests/ 文件夹。
- 错误类型/异常按语义区分：传输错误、HTTP 状态错误、解码错误、远程错误、校验错误（fail closed —— 请求发送前本地校验）。
