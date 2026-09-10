# RFB SDK（Python）

RFB 统一 SDK 的 Python 实现（Python ≥ 3.9，本仓按 3.12 验证），**零第三方依赖**（仅标准库：`http.client` / `socket` / `json` / `struct` / `unittest` 等），同步风格 API。

## 覆盖范围

- **公共 API（UNIFIED_API.md §1 全集）**：`RfbClient`（唯一客户端）、`Sandbox`、`GuestStream`、`StreamEvent` / `StreamEventKind`、`ExecResult`、`DirEntry` / `GrepMatch` / `FileRead`、`Snapshot` / `SandboxInfo`、`RfbError` 及五个子类 `TransportError` / `HttpStatusError` / `DecodeError` / `RemoteError` / `ValidationError`。`rfb_sdk.__init__` 只导出这些类型。
- **内部协议适配器（下划线模块，不进入公共 API）**：
  - `_forkd.py` — forkd controller HTTP/JSON（`GET /v1/snapshots`、`GET /v1/snapshots/{tag}/info` → 旧端点回退、双 404 = `None`、`POST /v1/sandboxes`、`GET /v1/sandboxes`、`POST /v1/sandboxes/{id}/ping`、`DELETE /v1/sandboxes/{id}`（2xx/404 均成功）；Bearer 认证、非 2xx 错误取 JSON `error` 字段或响应体前 1024 字符、sandbox-id 校验 `[A-Za-z0-9_-]{1,128}`）。
  - `_guest.py` — forkd guest TCP NDJSON（动作 ping/exec/eval/ls/find/grep/read/write/stream；逐行读到终结行；1 MiB 行上限；`error` 行 → `RemoteError`；§2.3 全部规则发送前本地校验 fail closed）。
  - `_zbrt.py` — ZBRT v1 二进制帧（28 字节头、strict payload codec 拒绝截断与尾部多余字节、legacy Cancel、会话语义：Execute → 0..n Output → 恰好一个 Exit/Error、幂等 Cancel → 空 payload CancelAck、Health → HealthAck、Fs opcode 1-5、未知 kind → Error(code=1)、每请求新 128-bit request_id）。
- **传输**：`create_sandbox(..., transport="ndjson"|"zbrt")` 与 `connect(..., transport=...)`；两种传输下 `Sandbox` 方法名与返回形状完全一致。
- **测试**：PROTOCOL.md §4 的 8 个黄金向量编码+解码双向命中；strict-decode 拒绝用例（坏 magic、错版本、非零 flags、未知 kind、截断、尾部多余字节、超长 payload）；fake HTTP / fake NDJSON TCP / fake ZBRT 帧服务器；两条传输的 facade 形状一致性。

## 安装 / 构建

无需构建。把 `sdk/python` 目录加入 `PYTHONPATH` 或直接在该目录下使用：

```bash
cd sdk/python
python -c "import rfb_sdk; print(rfb_sdk.RfbClient)"
```

## 统一 API 快速上手

> 本 SDK 是 Rust 参考实现 `rfb::client::RfbClient` 的 Python 镜像移植，逐方法对照；字节层契约见 [`../PROTOCOL.md`](../PROTOCOL.md)，API 表面见 [`../UNIFIED_API.md`](../UNIFIED_API.md)。

```python
from rfb_sdk import RfbClient

# base_url 缺省取环境变量 FORKD_URL（默认 http://127.0.0.1:8889），
# token 缺省取 FORKD_TOKEN（非空才带 Bearer 头），超时默认 10 秒。
client = RfbClient()                      # 或 RfbClient(base_url=..., token=..., timeout_s=10.0)

client.wait_snapshot("base")              # 轮询直至 ready（默认 60s；failed 立即抛 RemoteError）
sandboxes = client.create_sandbox("base", n=1)   # -> [Sandbox]（transport="ndjson" 默认）
sandbox = sandboxes[0]

result = sandbox.exec(["echo", "hello"], cwd="/", timeout_s=60.0, stdin=b"")
print(result.exit_code, result.stdout_text)

assert sandbox.write("/workspace/notes.txt", b"hi rfb") == 7      # -> bytes_written
data = sandbox.read("/workspace/notes.txt")                        # -> FileRead(data, truncated, total_bytes?)
print(data.data)                                                   # b"hi rfb"

print(sandbox.ls())          # [DirEntry(name, is_dir, size?)]
print(sandbox.find(pattern="*.py"))
print(sandbox.grep(pattern="rfb"))     # [GrepMatch(path, line?, column?, text)]
print(sandbox.ping())                  # True

sandbox.delete()                       # 2xx/404 均成功
```

ZBRT 传输（同样的方法与返回形状）：

```python
sandbox = client.create_sandbox("base", transport="zbrt")[0]
result = sandbox.exec(["echo", "hello"])
```

## 高级说明

- **流式执行**：`stream = sandbox.stream(args, cwd=None, pty=None, env=None)`；
  `stream.next_event()` 依次返回 `StreamEvent(kind=started|stdout|stderr|exit, data, code)`，干净关闭返回 `None`；
  `stream.send_input(text)`（NDJSON 写入 `{"in": ...}`；终结后调用抛 `RemoteError`）；
  `stream.stop()` 幂等（NDJSON 发送 `{"action":"stop"}`；ZBRT 发送 Cancel → 空 payload CancelAck，目标为当前活跃 request_id）。
  ZBRT 传输下 `pty=True` 或非空 `env` 在发送任何帧前抛 `ValidationError`（与 Rust 基线 fail-closed 行为一致；`pty=False` 与空 `env` 不受影响）。
- **错误体系**（§7）：`TransportError`（连接/读写/超时）、`HttpStatusError`（携带 `status` + `message`）、`DecodeError`（响应/帧解码失败，含 strict codec 拒绝）、`RemoteError`（guest error 行、ZBRT Error 帧、forkd error 字段）、`ValidationError`（发送前本地校验失败，fail closed）。
- **校验规则**（PROTOCOL.md §2.3，上限 4096 / 1024 / 1000 / 51200 / 1048576）在请求发出前于本地执行，失败抛 `ValidationError`，绝不产生网络流量。
- **快照便捷语义**：`client.snapshot(tag)` 为 `/info` → 旧端点回退链，双 404 返回 `None`；`client.wait_snapshot(tag, timeout_s=60)` 每 100ms 轮询，`failed`（忽略大小写）立即抛 `RemoteError`，超时抛 `TransportError`。
- **黄金向量**：`tests/test_zbrt_codec.py` 对 PROTOCOL.md §4 全部 8 个向量做编码+解码双向断言。

## 运行测试

```bash
cd sdk/python
python -m unittest discover -s tests -v
```

测试全部使用进程内 fake 服务器（fake controller HTTP、fake guest NDJSON、fake ZBRT 帧服务），无外部依赖，快速执行。

## 已知限制

- **同步阻塞 I/O**：全部方法为同步调用，无异步变体（与 Rust 基准的 tokio 异步不同，属语言习惯差异）。
- **ZBRT 传输下的 `send_input`**：ZBRT v1 只在 Execute 帧内携带 stdin，没有向已提交请求追加 stdin 的 wire 消息，因此 `GuestStream.send_input` 在 ZBRT 传输下抛 `RemoteError`（`pty` / `env` 参数同样仅 NDJSON 支持）。
- **ZBRT 传输下的 `eval`**：ZBRT v1 没有独立 eval 原语，SDK 将 `eval(code)` 内部映射为一轮 Execute 帧：`argv=["eval", code]`（argc=2）、stdin 为空、`timeout_s=None` → `timeout_ms=0`（否则整秒×1000，u32 上限封顶）。该约定与 Rust 基线及 `../shared/conformance/eval_zbrt_vectors.json` 黄金向量一致（`tests/test_zbrt.py` 覆盖）。
- **`find` / `grep` 的 `pattern` 参数**为仅关键字参数（`sandbox.find(pattern="x")`），因为规范中 `path` 带默认值且排在 `pattern` 之前。
- Windows 下运行测试时，fake 服务器线程可能因客户端带未读数据关闭连接而打印连接重置信息，属正常清理路径，不影响结果。
- 帧超长（> 16 MiB payload）、行超长（> 1 MiB）等一律 fail closed，不做任何截断降级。

## 相关文档

- 字节层协议契约：[`../PROTOCOL.md`](../PROTOCOL.md)
- 统一 API 规范与验收清单：[`../UNIFIED_API.md`](../UNIFIED_API.md)
- Rust 基准实现：[`../../rfb/src/client/`](../../rfb/src/client/)（本 SDK 为其镜像移植）
