# RFB SDK（C#）

RFB 统一 SDK 的 C# 实现（.NET 8，`net8.0`，仅依赖 BCL：`System.Net.Sockets` / `System.Text.Json` 等，零第三方 NuGet 包），异步风格 API（`async`/`await` + `Task`，PascalCase 命名）。

## 覆盖范围

- **公共 API（UNIFIED_API.md §1 全集）**：`RfbClient`（唯一客户端）、`Sandbox`、`GuestStream`、`StreamEvent` / `StreamEventKind`、`ExecResult`、`DirEntry` / `GrepMatch` / `FileRead`、`Snapshot` / `SandboxInfo`、`RfbException` 及五个子类 `TransportException` / `HttpStatusException` / `DecodeException` / `RemoteException` / `ValidationException`。协议适配类全部为 `internal`，不进入公共 API。
- **内部协议适配器（`internal` 类，不进入公共 API）**：
  - `Internal/ForkdControllerHttp.cs` — forkd controller HTTP/JSON（`GET /v1/snapshots`、`GET /v1/snapshots/{tag}/info` → 旧端点回退、双 404 = `null`、`POST /v1/sandboxes`、`GET /v1/sandboxes`、`POST /v1/sandboxes/{id}/ping`、`DELETE /v1/sandboxes/{id}`（2xx/404 均成功）；Bearer 认证、非 2xx 错误取 JSON `error` 字段或响应体前 1024 字符、sandbox-id 校验 `[A-Za-z0-9_-]{1,128}`）。
  - `Internal/ForkdGuestNdjson.cs` — forkd guest TCP NDJSON（动作 ping/exec/eval/ls/find/grep/read/write/stream；逐行读到终结行；1 MiB 行上限；`error` 行 → `RemoteException`；§2.3 全部规则发送前本地校验 fail closed）。
  - `Internal/ZbrtFrameCodec.cs` + `Internal/ZbrtTcpClient.cs` — ZBRT v1 二进制帧（28 字节头、strict payload codec 拒绝截断与尾部多余字节、legacy Cancel、会话语义：Execute → 0..n Output → 恰好一个 Exit/Error、幂等 Cancel → 空 payload CancelAck、Health → HealthAck、Fs opcode 1-5、未知 kind → Error(code=1)、每请求新 128-bit request_id）。
- **传输**：`CreateSandbox(..., transport: "ndjson"|"zbrt")`、`ListSandboxes(transport: ...)`、`Connect(..., transport: ...)`；两种传输下 `Sandbox` 方法名与返回形状完全一致。
- **测试**：PROTOCOL.md §4 的 8 个黄金向量编码+解码双向命中；`sdk/shared/conformance/eval_zbrt_vectors.json` 的 eval-over-ZBRT 黄金向量（编码方向 + 本地校验拒绝 0 帧）；strict-decode 拒绝用例（坏 magic、错版本、非零 flags、未知 kind、截断、尾部多余字节、超长 payload）；fake HTTP / fake NDJSON TCP / fake ZBRT 帧服务器；两条传输的 facade 形状一致性。

## 构建 / 安装

.NET 8 SDK，无需额外还原第三方包：

```bash
cd sdk/csharp
dotnet build Rfb.Sdk.slnx
```

## 统一 API 快速上手

> 本 SDK 是 Rust 参考实现 `rfb::client::RfbClient` 的 C# 镜像移植，逐方法对照；字节层契约见 [`../PROTOCOL.md`](../PROTOCOL.md)，API 表面见 [`../UNIFIED_API.md`](../UNIFIED_API.md)。

```csharp
using Rfb.Sdk;

// baseUrl 缺省取环境变量 FORKD_URL（默认 http://127.0.0.1:8889），
// token 缺省取 FORKD_TOKEN（非空才带 Bearer 头），超时默认 10 秒。
var client = new RfbClient();   // 或 new RfbClient(baseUrl: ..., token: ..., timeoutS: 10.0)

await client.WaitSnapshot("base");                       // 轮询直至 ready（默认 60s；failed 立即抛 RemoteException）
var sandboxes = await client.CreateSandbox("base", n: 1); // -> IReadOnlyList<Sandbox>（transport: "ndjson" 默认）
var sandbox = sandboxes[0];

var result = await sandbox.Exec(new[] { "echo", "hello" }, cwd: "/", timeoutS: 60.0);
Console.WriteLine($"{result.ExitCode} {result.StdoutText}");

long written = await sandbox.Write("/workspace/notes.txt", "hi rfb"u8.ToArray()); // -> bytes_written
FileRead data = await sandbox.Read("/workspace/notes.txt");                       // -> FileRead(Data, Truncated, TotalBytes?)
Console.WriteLine(System.Text.Encoding.UTF8.GetString(data.Data));                // "hi rfb"

foreach (var entry in await sandbox.Ls())          // IReadOnlyList<DirEntry>(Name, IsDir, Size?)
    Console.WriteLine(entry.Name);
Console.WriteLine(string.Join("\n", await sandbox.Find(".", "*.cs")));   // path 在前（UNIFIED_API §4）
foreach (var m in await sandbox.Grep(".", "rfb"))   // IReadOnlyList<GrepMatch>(Path, Line?, Column?, Text)
    Console.WriteLine(m.Text);
Console.WriteLine(await sandbox.Ping());           // True

await sandbox.Delete();                            // 2xx/404 均成功
```

ZBRT 传输（同样的方法与返回形状）：

```csharp
var sandbox = (await client.CreateSandbox("base", transport: "zbrt"))[0];
var result = await sandbox.Exec(new[] { "echo", "hello" });
```

## 高级说明

- **流式执行**：`var stream = await sandbox.Stream(args, cwd: null, pty: null, env: null);`
  `await stream.NextEvent()` 依次返回 `StreamEvent(Kind = Started|Stdout|Stderr|Exit, Data, Code?)`，干净关闭返回 `null`；
  `await stream.SendInput(text)`（NDJSON 写入 `{"in": ...}`；终结后调用抛 `RemoteException`）；
  `await stream.Stop()` 幂等（NDJSON 发送 `{"action":"stop"}`；ZBRT 发送 Cancel（reason `"stop"`，目标为当前活跃 request_id）→ CancelAck 被跳过，终结 `Exit` 事件经 `NextEvent()` 返回）。
- **错误体系**（§7）：`TransportException`（连接/读写/超时）、`HttpStatusException`（携带 `Status` + `Message`）、`DecodeException`（响应/帧解码失败，含 strict codec 拒绝）、`RemoteException`（guest error 行、ZBRT Error 帧、forkd error 字段）、`ValidationException`（发送前本地校验失败，fail closed）。
- **校验规则**（PROTOCOL.md §2.3，上限 4096 / 1024 / 1000 / 51200 / 1048576）在请求发出前于本地执行（`Internal/GuestValidation.cs`），失败抛 `ValidationException`，绝不产生网络流量。
- **快照便捷语义**：`client.Snapshot(tag)` 为 `/info` → 旧端点回退链，双 404 返回 `null`；`client.WaitSnapshot(tag, timeoutS: 60)` 每 100ms 轮询，`failed`（忽略大小写）立即抛 `RemoteException`，超时抛 `TransportException`。
- **黄金向量**：`tests/Rfb.Sdk.Tests/GoldenVectorTests.cs` 对 PROTOCOL.md §4 全部 8 个向量做编码+解码双向断言。

## 运行测试

```bash
cd sdk/csharp
dotnet test Rfb.Sdk.slnx
```

测试全部使用进程内 fake 服务器（fake controller HTTP、fake guest NDJSON、fake ZBRT 帧服务，见 `tests/Rfb.Sdk.Tests/Fakes.cs`），无外部依赖，快速执行。

## 已知限制

- **NDJSON 传输下 `Exec` 不支持 stdin**：forkd guest 协议不携带 stdin，传入非空 `stdin` 抛 `ValidationException`（ZBRT 传输的 Execute 帧内可携带 stdin）。
- **ZBRT 传输下的 `SendInput`**：ZBRT v1 只在 Execute 帧内携带 stdin，没有向已提交请求追加 stdin 的 wire 消息，因此 `GuestStream.SendInput` 在 ZBRT 传输下抛 `RemoteException`（`pty` / `env` 参数同样仅 NDJSON 支持；ZBRT 下传非空 `env` 或 `pty: true` 均抛 `ValidationException`，fail closed）。
- **ZBRT 传输下的 `Eval`**：ZBRT v1 没有独立 eval 原语，SDK 将 `eval(code)` 内部映射为 Execute 帧（`argv=["eval", code]`，见 `../shared/README.md` 四语言契约与 `../shared/conformance/eval_zbrt_vectors.json` 黄金向量）；这是 SDK 内部决策，wire 格式本身严格遵循 PROTOCOL.md。
- **`Find` / `Grep` 的参数顺序**：与 Rust/Python 基线统一为 path 在前（`sandbox.Find(path: ".", pattern: "*.cs")`），`path` 默认 `"."`；另提供单参数便捷重载 `Find("*.cs")` / `Grep("rfb")`（等价于 `path="."`）。
- **超时校验**：`Exec` / `Eval` 的 `timeoutS` 与客户端构造超时必须为有限且 > 0 的秒数，否则抛 `ValidationException`（fail closed）；ZBRT deadline 按秒向上取整 ×1000，与 NDJSON `TimeoutSecs` 一致。
- 帧超长（> 16 MiB payload）、行超长（> 1 MiB）等一律 fail closed（strict codec 拒绝），不做任何截断降级。

## 相关文档

- 字节层协议契约：[`../PROTOCOL.md`](../PROTOCOL.md)
- 统一 API 规范与验收清单：[`../UNIFIED_API.md`](../UNIFIED_API.md)
- Rust 基准实现：[`../../rfb/src/client/`](../../rfb/src/client/)（本 SDK 为其镜像移植）
