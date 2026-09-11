# RFB Java SDK

RFB 的 Java 外部语言 SDK，是 **Rust 基准实现（`rfb::client`）的镜像移植**。提供唯一的公共客户端类型 `RfbClient`，覆盖三种 RFB wire 协议：

| 协议 | 传输 | 内部实现（不进入公共 API） |
|---|---|---|
| forkd controller | HTTP/JSON REST | `io.rfb.sdk.internal.ControllerHttp` |
| forkd guest | TCP + 换行分隔 JSON（NDJSON） | `io.rfb.sdk.internal.GuestNdjson(+Stream)` |
| ZBRT v1 | 二进制帧（28 字节帧头） | `io.rfb.sdk.internal.ZbrtFrame/Codec/Connection` |

- **wire 契约唯一依据**：[`../PROTOCOL.md`](../PROTOCOL.md)（帧格式、字段表、限制、校验规则、黄金测试向量）。
- **公共 API 表面唯一依据**：[`../UNIFIED_API.md`](../UNIFIED_API.md)（四语言统一：Rust 基准，Java 为镜像移植）。
- **Java 8**（`maven.compiler.release=8`，产物 class 版本 52），唯一依赖 `com.fasterxml.jackson.core:jackson-databind 2.17.x`，HTTP 用 `HttpURLConnection`，TCP 用原生 Socket。测试模块（`tests/`）单独以 JDK 17 编译运行（仅测试代码需要）。

## 公共 API 全集（与 UNIFIED_API.md §1 一致）

```
RfbClient / Sandbox / GuestStream / StreamEvent
ExecResult / DirEntry / GrepMatch / FileRead
Snapshot / SandboxInfo
RfbError + TransportError / HttpStatusError / DecodeError / RemoteError / ValidationError
```

协议实现全部在 `io.rfb.sdk.internal` 包（javadoc 标注 internal），公共导出不含任何 `Forkd*` / `Zbrt*` 类型。

## 构建与安装

需要 JDK 17+ 与 Maven（本 SDK 的开发机上未安装 JDK/Maven，主代码的正确性经过静态审查，测试未实际执行）：

```bash
cd sdk/java
mvn -q compile            # 主代码 io.github.cricle:rfb-sdk（src/main/java）
mvn -q test               # 测试（tests/ 模块，JUnit 5，全部进程内 fake）
```

`pom.xml` 产出主 jar（`io.github.cricle:rfb-sdk`，Java 8 target）；`tests/pom.xml`（`io.github.cricle:rfb-sdk-tests`，JDK 17）以 `<scope>test</scope>` 依赖主模块并在 surefire 下运行 JUnit 5 套件。发行流程见 `docs/RELEASE.md`（Maven Central 渠道）。

## 快速上手

### 0. 建沙箱 → 执行 → 读写 → 删除（UNIFIED_API.md §9 统一场景）

```java
import io.rfb.sdk.*;

RfbClient client = new RfbClient();                     // FORKD_URL / FORKD_TOKEN 环境变量
Snapshot snap = client.waitSnapshot("base");            // 轮询 100ms 直到 ready

Sandbox box = client.createSandbox("base").get(0);
ExecResult r = box.exec(List.of("echo", "hi"));         // NDJSON 传输（默认）
System.out.println(r.stdoutText());

box.write("notes.txt", "hello".getBytes(StandardCharsets.UTF_8));
FileRead back = box.read("notes.txt");

box.delete();
```

### 1. RfbClient（controller 门面）

```java
RfbClient client = new RfbClient();                          // 或 new RfbClient(baseUrl, token, timeoutS)
List<Snapshot> snapshots = client.listSnapshots();
Snapshot detail = client.snapshot("base");                   // /info → 旧端点回退，双 404 = null
List<Sandbox> boxes = client.createSandbox("base", 2, false, 512L, true, false, false);
Object pong = client.pingSandbox("sb-1");
client.deleteSandbox("sb-1");                                // 2xx / 404 均成功
```

默认值：`baseUrl=null` → 环境变量 `FORKD_URL`（默认 `http://127.0.0.1:8889`）；`token=null` → 环境变量 `FORKD_TOKEN`（非空才带 `Authorization: Bearer`）；超时 10 秒。

### 2. Sandbox（guest 门面：NDJSON / ZBRT 两种传输，方法与返回形状完全一致）

```java
Sandbox box = client.connect("sb-1");                       // 默认 NDJSON
Sandbox zb   = client.connect("sb-1", "zbrt");              // ZBRT v1 二进制帧

boolean ok = box.ping();                                    // healthy
ExecResult r = box.exec(List.of("ls", "-l"), "/workspace", 5.0);
ExecResult e = box.eval("print(1)");                        // eval 的 output 映射为 stdout
List<DirEntry> entries = box.ls(".");                       // 仅名匹配：find / grep 同理
List<String> files = box.find(".", "*.txt");
List<GrepMatch> hits = box.grep(".", "TODO");
FileRead fr = box.read("notes.txt", null, 4096);
int n = box.write("notes.txt", "data".getBytes(), false, null);

try (GuestStream s = box.stream(List.of("tail", "-f", "x"))) {
    StreamEvent ev;
    while ((ev = s.nextEvent()) != null && !"exit".equals(ev.kind)) {
        s.sendInput("ping\n");                              // 仅 NDJSON 传输支持
    }
}
```

两种传输下 `exec`/`eval`/`ls`/`find`/`grep`/`read`/`write`/`stream`/`ping` 的方法名与结果字段一致（eval 的 `output` 统一映射为 `ExecResult.stdout`）。

### 3. 错误处理

```java
try {
    box.read("C:/etc/passwd");
} catch (ValidationError e) { /* 本地校验失败（fail closed，未发包） */ }
catch (RemoteError e)      { /* 对端 error 行 / ZBRT Error 帧 */ }
catch (HttpStatusError e)  { /* forkd 非 2xx：e.getStatus() */ }
catch (DecodeError e)      { /* 响应 / 帧解码失败 */ }
catch (TransportError e)   { /* 连接 / 读写失败、超时 */ }
```

所有 PROTOCOL.md §2.3 校验（路径 4096B、pattern 1024B、结果数 1000、负载 51200B、代码 1MiB、eval/cancel-id 规则、sandbox-id 规则）都在发送前本地执行，失败抛 `ValidationError`，绝不把非法请求发出去。

## 运行测试

```bash
cd sdk/java/tests
mvn test
```

测试全部为进程内 fake（无外部服务、无长 sleep）：

| 测试类 | 覆盖 |
|---|---|
| `ZbrtFrameCodecTest` | PROTOCOL.md §4 全部 8 个黄金向量（编码命中 hex + 解码命中结构，双向）；坏 magic/版本/flags/未知 kind/截断帧头帧体/超限 payload 严格拒绝 |
| `ZbrtCodecTest` | 各 payload 严格编解码（截断 + 尾部多余字节拒绝）、legacy Cancel（无 target 字节）、带 target Cancel |
| `ControllerHttpTest` | 进程内 fake HTTP：快照列表、`/info` → 旧端点回退链、错误映射（JSON error 字段 / 前 1024 字符）、Bearer 头、delete 404=成功、sandbox-id 校验、请求体字段名 |
| `GuestNdjsonTest` | 进程内 fake TCP NDJSON：多行读到终结行、error 行抛出、exec/eval/ls/find/grep/read/write 映射、流会话 sendInput/stop/exit、响应 50KiB 上限、§2.3 校验 fail closed |
| `ZbrtConnectionTest` | 进程内 fake ZBRT：Output 流 → 恰一个终结帧、Error 帧、幂等 Cancel（空 payload CancelAck）、HealthAck、fs 读写 round-trip、第二个 Execute / 重复 request-id 错误、读停顿超时抛 TransportError、request_id 不匹配 |
| `ZbrtClientTest` | eval-over-ZBRT 一致性黄金向量（`sdk/shared/conformance/eval_zbrt_vectors.json`）：EVAL_ZBRT_BASIC / EVAL_ZBRT_DEFAULTS 逐字节帧 hex + 字段断言、校验失败（空 code、timeout=0）零发包 |
| `RfbClientFacadeTest` | 统一门面：create → exec → read/write → delete 全流程、connect/pingSandbox、传输名校验、connect(Sandbox) 直接附着、createSandbox transport 参数 |
| `ValidationTest` / `DtoJsonTest` | §2.3 全部规则（UTF-8 字节长度）、§1.3 JSON 字段名与 serde(default) 默认值 |

## 统一 API 说明

- 四语言（rust/python/csharp/java）公共类型与方法一一对应；Rust 是基准实现，本 SDK 是镜像移植，语义差异仅限语言习惯（Java 小驼峰、同步阻塞、unchecked 异常）。
- 公共类型集合严格限定为 UNIFIED_API.md §1 全集（上表）：曾经导出的 `CreateSandboxRequest` / `CreateOptions` / `Transport` 均为死代码或不在全集内，已删除；传输选择沿用既有惯例以字符串 `"ndjson"` / `"zbrt"` 表示（`connect` / `createSandbox` 同）。
- `createSandbox(...)` 支持末位可选 `transport` 参数（重载提供，默认 NDJSON），决定返回的 `Sandbox` 门面使用的 guest 传输；`connect(sandbox)` 直接附着传入的 `Sandbox` 对象（保留其传输），仅 `connect(String id)` 走 listSandboxes 重解析。
- `exec(args, cwd, timeoutS, stdin)`：`args` 不得为空、`cwd` 需为合法 guest 路径（两传输均在发送前本地校验，fail closed）；NDJSON 传输的 wire 契约没有 exec stdin 通道，非空 stdin 静默丢弃（与 Rust 基准一致；stdin 仅 ZBRT 送达）；默认 cwd 解析为 guest 根（NDJSON 发送 `/workspace`，ZBRT 省略 cwd 字段）——forkd guest 会拒绝 `"/"`。NDJSON 响应缺 `exit_code` 时回退 `-1`（与 Rust 一致）。
- `eval`：两种传输均支持。ZBRT v1 无 eval opcode，统一约定（`sdk/shared/README.md`）编码为一轮 `Execute`：`argv=["eval", code]`、stdin 为空、`timeout_ms` = 整秒数 ×1000（未指定时为 0）；eval 的 `output` 统一映射为 `ExecResult.stdout`（NDJSON 下 stderr 恒为空、缺 status 回退 0）。一致性黄金向量见 `sdk/shared/conformance/eval_zbrt_vectors.json`（测试：`ZbrtClientTest::evalZbrtMatchesSharedVector`）。
- `waitSnapshot` 轮询间隔 100ms；`status=failed` 立即抛 `RemoteError`；超时抛 `TransportError`（UNIFIED_API.md §7：超时属于传输类错误）。
- ZBRT fs 帧 data JSON 按 PROTOCOL.md §3.3 恒带全部键：read 为 `{"offset":..,"max_bytes":..}`（缺省为 null）、write 为 `{"data":[..],"append":..,"mode":..}`（缺省 mode 为 null）。
- ZBRT 传输的 exec 超时只作为随帧下发的 guest 侧 deadline；客户端读停顿超时直接抛 `TransportError`，不做客户端主动 Cancel（与 Rust 基准一致）。
- `GuestStream.sendInput`：仅 NDJSON 支持；ZBRT v1 的 stdin 只能随 `Execute` 静态下发。
- `GuestStream.stop`：NDJSON 发送 `{"action":"stop"}`；ZBRT 发送带 target 的 Cancel（空 payload CancelAck，幂等）。

## 已知限制

- 验证状态：主模块 `mvn compile` 以 `release=8` 编译通过（class 文件 major version 52）；测试模块 108 个 JUnit 5 用例全绿（WSL JDK 21 实测）。
- ZBRT `Result`（kind 13，保留帧）未实现 legacy 兼容解析（Rust 基准可解析旧版单帧 Result）；收到时抛 `DecodeError`。
- ZBRT 传输的 `stream` fail closed：`pty=true` 或非空 `env` 在发送任何帧前抛 `ValidationError`（与 Rust/C#/Python 基线一致，不再忽略；ZBRT v1 无这两个通道）；空 `args` 在两种传输下同样拒绝。
- NDJSON 传输单响应行上限 1 MiB、结构化工具响应 50 KiB / 1000 条上限，超出抛错（与 Rust 客户端一致）。
- 每次 guest/ZBRT 操作新建 TCP 连接（NDJSON 协议本身如此；ZBRT 为简化连接状态管理），流会话除外（单连接存活至终结）。
