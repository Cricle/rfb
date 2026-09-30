# sdk/PROTOCOL.md — 字节层 wire 契约（v1）

规格来源：Rust 基准实现（`rfb::client`、`rfb-runtime`）。四个 SDK（Rust / Python / C# / Java）
对同一公共 API 方法必须产生**逐字节一致**的 wire 行为；分歧以本文件与 `sdk/shared/` 的
黄金向量为准修正。本文件只描述“实际在跑”的行为。

## 0. 三种 wire 协议总览（互不可换）

| 协议 | 载体 | 使用方 | 对应 rootfs（不可混用） |
|---|---|---|---|
| forkd controller | HTTP/JSON | 所有 SDK 的 `RfbClient`（快照/沙箱生命周期） | — |
| forkd agent（NDJSON） | TCP（guest 端口 8888），一行一个 JSON | SDK 默认 guest 传输 `transport="ndjson"` | `forkd-agent.ext4` |
| ZBRT v1（ZeroBoot 帧） | vsock 5000 → TCP relay，二进制帧 | SDK guest 传输 `transport="zbrt"` | `zeroboot-zbrt.ext4` |
| RFB1（framed vsock） | vsock，`RFB1` 魔数二进制帧 | `rfb-cli rfb1` 验收 / host 侧 runtime 服务（**不是** SDK 传输） | `rfb-vsock.ext4` |

**三种 guest 协议不可互换**：帧格式、端口、动作集合、会话语义都不同；rootfs 按协议烧制
（`rfb-vsock` / `forkd-agent` / `zeroboot-zbrt` 三种 rootfs 不可互换，见 `sdk/BACKEND_SETUP.md`）。
同一 `Sandbox` 方法在 NDJSON 与 ZBRT 下同名同结果形状（`UNIFIED_API.md §4`），但 wire 编码完全不同。

## 1. forkd controller（HTTP/JSON）

### 1.1 标识符

sandbox id / cancel id：非空、≤ 128 字符、仅 `[A-Za-z0-9_-]`；URL path 中逐字 percent-encode。
snapshot tag 同样逐字 percent-encode 后拼入 path。

### 1.2 端点

| 方法与路径 | 语义 |
|---|---|
| `GET /v1/snapshots` | 快照列表（JSON 数组） |
| `GET /v1/snapshots/{tag}/info` | 快照详情；404 回退旧端点 `GET /v1/snapshots/{tag}`；**双 404 = None**（不是错误） |
| `POST /v1/sandboxes` | 创建沙箱（body 见 §1.3）；返回 `SandboxInfo` 数组 |
| `GET /v1/sandboxes` | 存活沙箱池 |
| `POST /v1/sandboxes/{id}/ping` | controller 侧 ping，原样返回 JSON 值 |
| `DELETE /v1/sandboxes/{id}` | 删除沙箱；**2xx 与 404 都算成功** |

认证：token 非空才带 `Authorization: Bearer <token>`。非 2xx 抛 HTTP 状态类错误
（`UNIFIED_API.md §7`），message 取 body JSON 的 `error` 字段，否则取前 1024 字符。客户端
维护一条 keep-alive 连接；复用中的陈旧连接（对端已关）**仅对幂等方法（GET/HEAD/DELETE）
自动重试一次**，POST 不重放（对端可能已执行，避免重复创建沙箱）。

### 1.3 DTO 字段名（serde(default)：缺失字段取默认值，不报错）

| Snapshot（默认 `""`/`false`/`null`） | SandboxInfo（默认同左） |
|---|---|
| `tag` `dir` `status` `bootable` | `id` `snapshot_tag` `guest_addr` |
| `created_at_unix` `branched_from` `pause_ms` | `created_at_unix` `netns` `pid` `memory_limit_mib` |
| `diff_ms` `diff_physical_bytes` `diff_logical_bytes` | `has_branched`(false) `branch_count`(0) |
| `warning` `digest` `provenance` | |

标量默认：字符串 `""`、`bootable`/`has_branched` `false`、`branch_count` `0`、其余 `null`。
创建沙箱 body（缺省即 false/null）：`{"snapshot_tag":…,"n":1,"per_child_netns":false,"memory_limit_mib":null,"prewarm":false,"live_fork":false,"hugepages":false}`。

## 2. forkd guest agent（TCP + NDJSON）

### 2.1 帧格式与终结

每次请求**新开一条 TCP 连接**（`TCP_NODELAY`）；请求 = 一行紧凑 JSON + `\n`；响应 = 一或多个
JSON 行，空行（keepalive）跳过。**任一行长度 > 1 MiB 或未以 `\n` 结尾 → Decode**。响应行含
§2.4 任一**终结键**即停止读取（取最后一行为结果）：`exit_code` `pong` `results` `entries`
`matches` `data` `content` `output` `status` `ok` `healthy` `done` `cancelled` `bytes_written`。
任一响应行含字符串型 `error` 键 → Remote（fail closed）；对端先关 → Remote；连接失败 → Transport。

读预算：带 `timeout` 的 `exec`/`eval` 响应读预算 = 客户端基准超时 + 请求 `timeout` 秒数
+ 5 s 余量（`EXEC_READ_MARGIN`，`rfb/src/forkd/guest.rs`；另见 `sdk/shared/README.md §3`），
保证 guest 自己的超时错误先于客户端读超时到达；未带 `timeout` 的请求读预算 = 基准超时。
若 agent 配置了认证，连接建立后的**首帧**必须先是 auth 行（见 §2.6），否则该连接上的任何
动作都会被拒绝。

### 2.2 动作（请求行字段）

| action | 请求字段 | 终结行字段 |
|---|---|---|
| `ping` | `{action}` | `pong: bool` |
| `exec` | `{action, cwd, args:[…], timeout}` | `exit_code:int\|null`、`stdout`、`stderr`、`timed_out`（旧 agent 用 `out`/`err` 键，SDK 两者都收） |
| `eval` | `{action, code, cwd?, timeout?}` | `output`（旧 `out`）、`status`、`timed_out` |
| `ls` | `{action, path, max_results}` | `entries:[{name,is_dir,size}]` |
| `find` | `{action, path, pattern, max_results}` | `matches:[string]` |
| `grep` | `{action, path, pattern, max_results, max_bytes}` | `matches:[{path,line,column,text}]` |
| `read` | `{action, path, offset?, max_bytes?}`（缺省键不上 wire） | `data`、`truncated`、`total_bytes?` |
| `write` | `{action, path, data:[u8…], append, mode?}` | `bytes_written:int` |
| `stream` | `{action, args, cwd?, pty?, env?}` | 事件行（见 §2.5） |

`timeout` 是**整秒**：RFB 时长为毫秒，SDK 统一 `ceil(秒)` 且最小 1。

### 2.3 发送前本地校验（fail closed，绝不产生网络流量；`rfb/src/guest/limits.rs` 镜像）

| 规则 | 上限 / 要求 |
|---|---|
| fs 路径（ls/find/grep） | 非空；UTF-8 ≤ 4096 字节；无 NUL；无 `\`；无 `..` 段；绝对路径仅允许 `/workspace` 或 `/workspace/…`（`/workspacefoo` **不在**工作区内） |
| 文件路径（read/write、eval/stream cwd） | 同上但允许任意绝对/相对路径；拒绝 `X:` 盘符 |
| pattern | 非空、无 NUL、≤ 1024 字节 |
| max_results / max_bytes | `> 0` 且 ≤ 1000 / 51200 |
| 写入负载 / eval 代码 | ≤ 51200 字节 / ≤ 1 MiB（eval 代码去空白后非空） |
| eval timeout / argv | 数字秒、`> 0`（0 拒绝）/ argv 非空（两传输一致拒绝） |

### 2.4 结果形状（NDJSON JSON 与 ZBRT FsResult JSON **同形**，见 §3.3）

`ls→entries`、`find→matches`（字符串）、`grep→matches`（对象）、`read→data/truncated/total_bytes`、
`write→bytes_written`、`exec→exit_code/stdout/stderr/timed_out`（缺 `exit_code` 或非整数 → `-1`）、
`eval→output→stdout 映射（exit 缺省 0，stderr 恒空）`、`ping→pong==true 才算健康`。字节值统一为
JSON 数组或 UTF-8 字符串（二者皆收）；缺省读作空。

### 2.5 stream 事件行（`forkd_stream_event` 映射）

顺序判定：`started`（`{"event":"started"}` 或 `{"started":true}`）→ `exit_code`（**键存在即终结**：
值为 null（子进程被信号杀死）→ Exit 无码终结，不是协议错误）→ `done:true`（Exit 无码）→ 输出键
`stdout`（旧 `out`）/ `stderr`（旧 `err`）→ 其余忽略。
输入：`{"in": text}`；停止：`{"action":"stop"}`（幂等）。

### 2.5a find 的 pattern 语义（所有传输、所有后端**单一来源**）

`pattern` 是 **glob 名字匹配**（实现：`rfb-runtime/src/glob.rs::glob_matches`，全部 find 走同一
walk）：

- `*` 匹配任意字节序列（**含路径分隔符**——匹配对象是逐条 walked 条目的名字，目录条目名也参与
  匹配）；
- 其余字符是字面量；无 `*` 的 pattern = **全名精确匹配**（`note` 不命中 `note.txt`——不是子串
  匹配）；
- 匹配按 walked 条目名进行（递归），结果为相对 `path` 的 `/` 分隔路径；
- `matches` 被 `max_results` 截断时 `truncated: true`（结果形状见 §2.4 的 `read` 同名字段；
  find 的 `matches`/`truncated` 同形状）。

### 2.6 agent 认证（可选 `FORKD_AGENT_TOKEN`）

触发：agent 启动时环境变量 `FORKD_AGENT_TOKEN` **非空**（空白视为未设）——此时每条新连接都
先过认证门；未设 = 历史开放行为，wire 无任何变化。

- **首帧**：客户端必须在连接建立后 **10 s**（`AUTH_TIMEOUT`）内发送一行紧凑 JSON
  `{"action":"auth","token":"<token>"}`；该帧不算业务动作。
- **成功**：agent 回 `{"action":"auth","ok":true}`，连接随后进入正常动作分发（§2.2）。
- **失败**：首帧非法 JSON 或 `action != "auth"` → `{"error":"authentication required"}`；
  token 不匹配 → `{"action":"auth","ok":false,"error":"authentication failed"}`；首帧超时 /
  EOF / 行超限 → 不回帧直接关闭。任一失败后连接关闭，业务动作一律不执行。

SDK 侧约定：需要访问受保护 agent 的客户端读同名环境变量，连接后先发 auth 行再发业务动作
（Python / Node.js 已实现；Rust / C# / Java 的接入状态见 `UNIFIED_API.md §10`）。

## 3. ZBRT v1（ZeroBoot 二进制帧）

### 3.1 帧头（28 字节，大端）

| 偏移 | 字段 | 值 |
|---|---|---|
| 0..4 | magic | `ZBRT` |
| 4 | version | `1`（不符 → Decode） |
| 5 | kind | 见 §3.2（未知 → Decode） |
| 6..8 | flags | **必须为 0** |
| 8..24 | request_id | 16 字节，每请求随机新生成 |
| 24..28 | payload_len | u32；> 16 MiB → Decode |

严格解码：截断、尾部多余字节、超限 payload 都拒绝。

### 3.2 kind 与 payload（字段一律 u32 BE 长度前缀，严格读写）

| kind | 名称 | payload |
|---|---|---|
| 1 / 2 | Hello / HelloAck | `client:str, cap_count:u8, caps:[str]` |
| 3 | Execute | `argc:u8, argv:[str], cwd_flag+cwd, stdin:bytes, timeout_ms:u32`（argv 超过 255 条时 SDK 侧**本地 Validation、零帧上线**，见 §3.4） |
| 4 | Output | `stream:u8(0=stdout,1=stderr), data:bytes` |
| 5 | Exit | `code:i32, signal_flag+signal:u32` —— **终结** |
| 6 / 7 | Cancel / CancelAck | `reason_flag+reason, target_flag+16B`（旧 payload 无 target 字节，解码为 None）；**CancelAck payload 必须为空** |
| 8 / 9 | Fs / FsResult | `op:u8(1=ls,2=find,3=grep,4=read,5=write), path:str, data:bytes(=JSON)`；结果 JSON 同 §2.4 |
| 10 / 11 | Health / HealthAck | `healthy:u8, message_flag+message` |
| 12 | Error | `code:u32, message:str` —— **终结，抛 Remote** |

`timeout_ms`：`timeout_s=None → 0`（无 deadline）；否则秒数 ×1000，超出 u32 时 C#/Python 钳到
`u32::MAX`、Java eval 丢弃 deadline（置 0）。会话中 Execute 之外的非终结帧 → Decode。

### 3.3 Fs data JSON 键恒在（null 表示缺省）

read：`{"offset":…,"max_bytes":…}`；write：`{"data":[…],"append":…,"mode":…}`；其余 op 同 §2.2。

### 3.4 会话语义

**连接后必须先 Hello（强制握手）**：每条连接的第一个帧必须是 `Hello`——`client` 名非空即可
由各 SDK 自定（Rust 基准 `rfb-sdk`，Python `rfb-sdk-python`；guest 只校验非空），capabilities
= `ZBRT_V1_CAPABILITIES` 全集（`execute` `stream` `deadline` `health` `cancel` `filesystem`）；
guest 回 request_id 匹配的 `HelloAck` 后连接才可用。**未握手的业务帧一律被拒**：
`Error(code=1, "protocol handshake required")`，不会到达 runtime service；客户端侧握手失败
（被拒 / 非 HelloAck / HelloAck 载荷非法）统一抛 Transport，业务帧一个不发。

请求 id 与连接拓扑：每个请求新 16 字节 request_id；回复的 request_id 必须匹配，否则 Decode。
请求可在新连接上发出，也可在同一连接上顺序复用——本仓库 Rust 基准：`exec`/`stream` 每个 turn
新连接，`health` 与 fs RPC（Fs/FsResult）复用一条惰性建立的**控制连接**；控制连接上的交换失败
→ 丢弃该连接、重连（重新 Hello）并把该请求重试一次，guest 回的 `Error` 是请求的确定答复（不
重试）。Output 帧严格先于恰好一个终结帧（Exit 或 Error）；一个 Execute turn 的 stdout+stderr
**聚合超过 16 MiB → Remote**（客户端护栏）。host 无自发超时取消：读停顿超过客户端超时 →
Transport（基准实现逐帧使用客户端超时，不因 guest-side `timeout_ms` 放宽；NDJSON 侧的 exec
读预算公式见 §2.1）。eval 无专用 opcode，且参考 guest 把 Execute 原样当 `exec` 执行——SDK 侧
`eval` 在 ZBRT 下**本地 fail closed**（ValidationError，零帧上线），见 `sdk/shared/README.md §1`。
stdin 只在 Execute payload 里——已提交的 turn 无法再注入输入（send_input 抛 Remote）。stop：
Cancel（reason=`"stop"`，target=本请求 id）→ 等空 CancelAck，期间到达的 Output 先缓冲、Exit
则视作已终结。

## 4. 黄金向量

向量文件唯一存放于 [`shared/conformance/zbrt_vectors.json`](shared/conformance/zbrt_vectors.json)
（schema、加载约定与新增流程见 [`shared/conformance/README.md`](shared/conformance/README.md)）：
8 个 ZBRT 向量 HELLO、HELLOACK、EXECUTE、OUTPUT、EXIT、CANCEL、CANCEL_LEGACY、ERROR，
request_id 统一 `000102030405060708090a0b0c0d0e0f`；另含 `rejects` 严格解码拒绝用例
（单字节翻转 `hello` 帧后 decode 必须失败）。每个 SDK 测试必须**从该文件就地加载**（不复制）
并**编码与解码双向**逐字节命中（Rust `rfb/tests/client_codec.rs`、Python `tests/test_zbrt_codec.py`、
Java `ZbrtFrameCodecTest`、C# `GoldenVectorTests`、Node.js `src/test/zbrt.test.ts`）。
新增/修改向量必须同步本节清单与 §3.2。

## 5. RFB1（framed vsock，runtime 协议；非 SDK 传输）

18 字节头：magic `RFB1`、opcode u8、flags u8（bit0 = payload zstd）、sequence u64 **LE**、
payload_len u32 LE；外层再加 u32 LE 长度前缀成帧。payload 为 postcard 编码，≥ 1024 字节自动
zstd（解压后仍受 16 MiB 上限约束）。客户端赋予 sequence，guest 原样回显。

| opcode | 消息 | 方向 |
|---|---|---|
| 1 / 11 | Hello `{protocol_version}`（版本必须等于 `PROTOCOL_VERSION`）/ HelloAck | host→guest / guest→host |
| 2 | Capabilities `{session_per_vm, writable_workspace}` | 双向 |
| 3 | StartTurn（`SessionRequest`：session_id + request_id + prompt） | host→guest |
| 4 | Cancel `{session_id, request_id}`（幂等；可由**另一条连接**发出） | host→guest |
| 5 | Event（`turn.started` / `turn.progress` / `terminal.output` / `turn.completed` / `turn.failed` / `turn.cancelled`，后三者为**终结事件**） | guest→host |
| 6 / 7 / 12 | ReadHostFile / ReadWorkspaceFile / → FileContent `{request_id, path, content}` | host→guest / guest→host |
| 8 / 13 | WriteWorkspaceFile → WriteAck（仅限工作区内，越界拒收） | host→guest / guest→host |
| 9 / 10 | Shutdown（回 `ShutdownAck` 后停止服务）/ Error `{request_id, message}`（终结） | host→guest / guest→host |

StartTurn 由 worker 线程执行，单 runtime 同时只服务一个活跃 turn；Cancel/Shutdown 在其它连接上
始终可服务。坏 magic、未知 opcode、非零 flags 残留位、截断、尾随字节一律 fail closed。
