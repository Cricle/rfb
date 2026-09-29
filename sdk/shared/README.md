# sdk/shared — 四语言 SDK 一致性契约（conformance）

本目录是 `sdk/` 下 Rust / Python / C# / Java 四个 SDK 的**共享一致性契约**所在。
规格来源：`sdk/PROTOCOL.md` + `sdk/UNIFIED_API.md`（v1，单客户端 `RfbClient`）；
基准实现：Rust `rfb::client`（`rfb/src/client/facade.rs`），其余三语言为镜像移植。

约束：任何 SDK 不得发明私有 wire 约定；同一公共 API 方法在四种语言下必须产生
**逐字节一致**的 wire 行为。发现分歧时，以本文档与 Rust 基准实现为准修正各语言。

---

## 1. eval 在 ZBRT 传输下 fail closed（当前约定）

**背景**：ZBRT v1（PROTOCOL.md §3）没有 eval opcode，只有 `Execute`（kind=3），
而参考 guest 把 `Execute.argv` 原样交给 workspace executor —— 早期四语言各自
“发明”过映射（Rust/C# 用 `argv=["eval", code]`，Python 用 `argv=[code]`，Java 抛错），
其中 `argv=["eval", code]` 这一支把 guest 的 `eval: not found`（exit 127）当作
**成功结果**返回，属于静默假成功。

**当前统一约定**：`Sandbox.eval(...)` 在 ZBRT 传输下**本地 fail closed**，
抛 ValidationError（"eval is not supported over the ZBRT transport"），
**零帧上线**。本地前置校验（code 非空白、≤1 MiB、cwd 合法、timeout_s 非 0）
先于传输判断执行，同样零帧。

NDJSON 传输下 eval 保持 `{"action":"eval","code":...}`，结果映射为
`ExecResult(status→exit_code, output→stdout, timed_out)`——`status` 是 agent 的当前
eval 键（PROTOCOL.md §2.4），`exit_code` 为历史别名，两者同时出现时 **status 优先**
（四语言一致）。

> 若将来 ZBRT 增加 eval opcode，先在 PROTOCOL.md 定义 wire 语义，再四语言同步实现，
> 不要恢复 argv 约定。

## 2. 一致性测试（各 SDK 必须包含）

fail-closed 断言：`eval` 在 ZBRT 下抛 ValidationError，且 fake guest 收到 0 帧、
0 连接。各语言对应测试：

- Rust：`rfb/tests/client_facade.rs::eval_zbrt_fails_closed_without_sending_frames`
- Python：`tests/test_zbrt.py::EvalZbrtFacadeTests::test_eval_fails_closed_without_sending_frames`
- C#：`tests/Rfb.Sdk.Tests/FacadeTests.cs::Eval_OverZbrt_FailsClosed`
- Java：`tests/src/test/java/io/rfb/sdk/ZbrtClientTest.java::evalOverZbrtFailsClosedWithoutSendingFrames`

## 3. 键序与超时一致性（回归清单）

| 契约 | 规则 | 覆盖测试 |
|---|---|---|
| exec stdout/stderr | 当前键 `stdout`/`stderr` 优先，历史键 `out`/`err` 兜底 | 各语言 exec 测试 |
| exec stdin | NDJSON 零通道：非空 stdin 本地 fail closed（仅 ZBRT 送达） | 各语言 stdin 拒绝测试 |
| eval 状态 | `status` 优先，`exit_code` 兜底 | 各语言 eval 测试 |
| eval 输出 | `output` 优先，`out` 兜底 | 各语言 eval 测试 |
| exec 默认超时 | forkd 用 `ForkdConfig.guest_timeout`（不再硬编码 10s）；zeroboot 用 `Config.timeout`（30s） | `rfb/tests/forkd_provider.rs`、`zeroboot_provider.rs` |
| forkd stream 读预算 | `guest_timeout + exec timeout + 5s margin`（与 exec 读预算同式） | `rfb/tests/forkd_guest.rs` |

## 4. 各语言现状

| 语言 | eval over ZBRT | 键序 |
|---|---|---|
| Rust（基准） | ✅ fail closed | ✅ 当前键优先 |
| C# | ✅ fail closed | ✅ 当前键优先 |
| Python | ✅ fail closed | ✅ 当前键优先 |
| Java | ✅ fail closed | ✅ 当前键优先 |

各语言 README 的「统一 API / 已知限制」章节已同步为本文契约。
