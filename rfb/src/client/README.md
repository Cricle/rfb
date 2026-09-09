# RFB 统一 SDK —— Rust 基准实现（`rfb::client`）

本模块是四语言统一 SDK（rust / python / csharp / java）的**基准实现（reference implementation）**：其余三个语言的 SDK 是对它的逐方法镜像移植。

- **公共 API 只有一个客户端类型 [`RfbClient`](../../src/client/facade.rs)**，完整公共类型集见 `sdk/UNIFIED_API.md` §1；
- **wire 契约唯一依据是 `sdk/PROTOCOL.md`**（帧布局、JSON 字段、限制、校验规则、黄金测试向量）；
- 三个协议实现（forkd controller HTTP、forkd guest NDJSON、ZBRT v1 帧）全部是**内部模块**（`pub(crate)`），不出现在公共 API 中。

## 覆盖范围

| 层 | 内容 |
|---|---|
| controller（HTTP/JSON） | `GET /v1/snapshots`、`GET /v1/snapshots/{tag}/info`（404 回退旧端点，双 404 = `None`）、`POST /v1/sandboxes`、`GET /v1/sandboxes`、`POST /v1/sandboxes/{id}/ping`、`DELETE /v1/sandboxes/{id}`（2xx/404 均成功）；`FORKD_URL`/`FORKD_TOKEN` 环境变量、Bearer 头、10s 默认超时、非 2xx 错误取 body JSON `error` 字段否则前 1024 字符、sandbox id 校验 `[A-Za-z0-9_-]{1,128}` |
| guest（NDJSON） | `ping/exec/eval/ls/find/grep/read/write/stream` 动作；逐行读取直到终结行（`exit_code`/`pong`/`entries`/… 之一）；`error` 行 → RemoteError；1 MiB 行上限；`/workspace` 前缀路径；§2.3 全部限制发送前本地校验（fail closed） |
| ZBRT v1（二进制帧） | 28 字节帧头（magic `ZBRT`、version 1、flags 必须 0、128-bit request_id、payload ≤ 16 MiB）；strict payload 编解码（拒绝截断与尾部多余字节；legacy Cancel 无 target 字节）；`Hello/HelloAck/Execute/Output/Exit/Cancel/CancelAck/Fs/FsResult/Health/HealthAck/Error`；Fs opcode 1=ls 2=find 3=grep 4=read 5=write；每次请求全新 128-bit request_id；未知 kind → `Error(code=1)` |
| 门面 | `RfbClient::from_env()/new()`；`list_snapshots/snapshot/wait_snapshot/create_sandbox/list_sandboxes/connect/ping_sandbox/delete_sandbox`；`Sandbox::{ping, exec, eval, ls, find, grep, read, write, stream, delete}`（NDJSON/ZBRT 两种传输下方法名与返回形状完全一致）；`GuestStream::{next_event, send_input, stop}`；`ExecResult/DirEntry/GrepMatch/FileRead/Snapshot/SandboxInfo`；错误五类 `Transport/Http/Decode/Remote/Validation` |

## 安装 / 构建

```bash
# 仓库根（rfb workspace）
cargo build -p rfb-sdk --features forkd,zeroboot
```

依赖 crate 内已有实现，未新增依赖：controller 复用 `rfb::controller::ForkdClient`，NDJSON 复用 `rfb::forkd_guest::ForkdGuestClient/ForkdGuestStream`，ZBRT 帧编解码复用 `rfb::protocol`（`rfb-runtime::zeroboot_protocol` 的 re-export）。MSRV 1.82，edition 2021，tokio 异步。

## 统一 API 快速上手（与其他语言 SDK 同一场景）

```rust
use rfb::client::{CreateOptions, GuestTransport, RfbClient};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), rfb::client::RfbError> {
    // 1. 客户端：默认读 FORKD_URL（缺省 http://127.0.0.1:8889）与 FORKD_TOKEN
    let client = RfbClient::from_env()?;
    // 或显式指定：RfbClient::new("http://127.0.0.1:8889", None, Duration::from_secs(10))?

    // 2. 创建沙箱（可选参数用 CreateOptions，默认 n=1）
    let sandbox = client
        .create_sandbox("my-snapshot", CreateOptions::default())
        .await?
        .into_iter()
        .next()
        .expect("at least one sandbox");

    // 3. 执行命令
    let result = sandbox.exec(&["echo", "hello rfb"], "/", 60.0, b"").await?;
    println!("exit={} stdout={}", result.exit_code, result.stdout_text());

    // 4. 读写文件（返回形状在 NDJSON / ZBRT 两种传输下完全一致）
    let n = sandbox
        .write("/workspace/hello.txt", b"hello", false, None)
        .await?;
    assert_eq!(n, 5);
    let file = sandbox.read("/workspace/hello.txt", None, None).await?;
    assert_eq!(file.data, b"hello");

    // 5. 删除沙箱
    sandbox.delete().await?;
    Ok(())
}
```

连接已存在的沙箱：`client.connect(&sandbox).await?` 或 `client.connect("sb-id").await?`（按 id 连接经 `list_sandboxes` 解析；ZBRT 传输用 `connect_id_with(id, GuestTransport::Zbrt)`）。

## 高级说明

- **传输选择**：`GuestTransport::{Ndjson, Zbrt}`（默认 Ndjson）。`CreateOptions.transport` 决定新建沙箱门面的传输；`Sandbox::with_transport()` 可切换已持有门面的传输；两种传输下 `Sandbox` 方法名与返回形状完全一致。
- **校验（fail closed）**：所有 §2.3 规则（路径 4096 / pattern 1024 / max_results 1000 / max_bytes 与 payload 51200 / eval 代码 1 MiB / eval timeout 非 0 / sandbox id 字符集）在发送前本地执行，失败抛 `RfbError::Validation`，请求不出进程。
- **错误映射**：`ForkdClientError/ForkdGuestError/ContractError` → `RfbError`；`wait_snapshot` 的 failed 状态抛 `Remote`，超时抛 `Transport`（`ErrorKind::TimedOut`）。
- **ZBRT 会话语义**：单连接单活跃请求；Execute → 0..n Output → 恰好一个 Exit（取消 code=-1）或 Error；Cancel 幂等（空 payload CancelAck）；`stream.stop()` 发送 `Cancel{target=request_id}`，后续 `next_event` 自动跳过 CancelAck 并消费 Exit(-1)。
- **流式事件**：`GuestStream::next_event()` 产出 `StreamEvent{kind: Started|Stdout|Stderr|Exit, data, code}`；终结后返回 `Ok(None)`（干净关闭）；`send_input` 在终结后抛 `Remote`；`stop` 幂等。

## 运行测试

```bash
# 全部客户端测试（黄金向量 + 门面，需 feature）
cargo test -p rfb-sdk --features forkd,zeroboot --lib --test client_codec --test client_facade

# 仓库完整门禁（默认 feature，默认并行）
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

测试全部使用进程内 fake（手写极小 HTTP controller、NDJSON guest、ZBRT 帧服务，均为阻塞式 `std::net` + `std::thread`，不启动任何真实 VM）：

- `rfb/tests/client_codec.rs`（16 个测试）：`sdk/PROTOCOL.md` §4 全部 8 个黄金向量 **编码+解码双向命中**；strict-decode 拒绝用例（坏 magic、错版本、非零 flags、未知 kind、截断、尾部多余字节、超限 payload）。
- `rfb/tests/client_facade.rs`（13 个测试）：controller 全端点（list、info 回退链、错误映射、Bearer 头、delete-404-成功）；guest 多行响应读到终结行、error 行抛错、stream 会话（started/stdout/输入/stop/exit）、校验 fail-closed；ZBRT Output→Exit、Error 帧抛错、CancelAck、HealthAck、Fs 往返；**两种传输下 exec/eval/ls/find/grep/read/write/ping/delete 形状一致性**（含 eval output→stdout）与错误类别。

## 已知限制

1. **feature 门控**：`rfb::client` 需要 `forkd` feature；ZBRT 传输需要 `zeroboot` feature（默认 feature 集为空，与 crate 内 `forkd`/`backend` 模块的既有约定一致）。
2. **NDJSON exec 不带 stdin**：`sdk/PROTOCOL.md` §2.2 的 `exec` 动作没有 stdin 字段，NDJSON 传输下 `exec` 的 `stdin` 参数被忽略（ZBRT 传输通过 `Execute.stdin` 传递）。
3. **ZBRT eval 是约定映射**：ZBRT v1 没有 eval 操作，门面将 `eval(code)` 编码为 `Execute{argv: ["eval", code]}` 的 Execute 帧（见 `sdk/PROTOCOL.md` §3.2 会话语义之外的宿主侧约定）。
4. **ZBRT 不支持 stdin 注入 / pty / env**：`GuestStream::send_input` 在 ZBRT 传输下抛 `Remote`；`stream(pty=Some(true))` 或非空 `env` 抛 `Validation`（fail closed）。
5. **README 位置**：受任务约束（只能创建 `rfb/src/client/` 与 `rfb/tests/` 内的文件），本 README 放在 `rfb/src/client/README.md`。

## 权威规范

- wire 契约：[`sdk/PROTOCOL.md`](../../sdk/PROTOCOL.md)
- API 表面：[`sdk/UNIFIED_API.md`](../../sdk/UNIFIED_API.md)
- 内部权威实现：`rfb/src/forkd/controller.rs`、`rfb/src/forkd/guest.rs`、`rfb/src/guest/*`、`rfb-runtime/src/zeroboot_protocol.rs`、`rfb-runtime/src/zeroboot_connection.rs`
