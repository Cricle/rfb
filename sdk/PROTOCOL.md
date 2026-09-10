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
维护一条 keep-alive 连接；**复用中的陈旧连接（对端已关）重试一次**再失败。

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

顺序判定：`started`（`{"event":"started"}` 或 `{"started":true}`）→ `exit_code`（终结，标记会话
结束）→ `done:true`（Exit 无码）→ 输出键 `stdout`（旧 `out`）/ `stderr`（旧 `err`）→ 其余忽略。
输入：`{"in": text}`；停止：`{"action":"stop"}`（幂等）。

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
| 3 | Execute | `argc:u8, argv:[str], cwd_flag+cwd, stdin:bytes, timeout_ms:u32`（argc>255 拒绝） |
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

每次请求新连接 + 新 16 字节 request_id；回复的 request_id 必须匹配，否则 Decode。Output 帧严格
先于恰好一个终结帧（Exit 或 Error）；host 无自发超时取消（读停顿按传输超时抛 Transport）。eval
无专用 opcode：按 `sdk/shared/README.md` 编码为 `argv=["eval", code]`、空 stdin 的 Execute。
stdin 只在 Execute payload 里——已提交的 turn 无法再注入输入（send_input 抛 Remote）。stop：
Cancel（reason=`"stop"`，target=本请求 id）→ 等空 CancelAck，期间到达的 Output 先缓冲、Exit
则视作已终结。

## 4. 黄金向量

8 个 ZBRT 向量：HELLO、HELLOACK、EXECUTE、OUTPUT、EXIT、CANCEL、CANCEL_LEGACY、ERROR，
request_id 统一 `000102030405060708090a0b0c0d0e0f`；每个 SDK 测试必须**编码与解码双向**逐字节
命中（Python `tests/test_zbrt_codec.py`、Java `ZbrtFrameCodecTest`、C# `GoldenVectorTests`）。
eval-over-ZBRT 另有共享向量 `sdk/shared/conformance/eval_zbrt_vectors.json`。

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
