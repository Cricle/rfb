# sdk/shared — 四语言 SDK 一致性契约（conformance）

本目录是 `sdk/` 下 Rust / Python / C# / Java 四个 SDK 的**共享一致性契约**所在。
规格来源：`sdk/PROTOCOL.md` + `sdk/UNIFIED_API.md`（v1，单客户端 `RfbClient`）；
基准实现：Rust `rfb::client`（`rfb/src/client/facade.rs`），其余三语言为镜像移植。

约束：任何 SDK 不得发明私有 wire 约定；同一公共 API 方法在四种语言下必须产生
**逐字节一致**的 wire 行为。发现分歧时，以本目录的向量与说明为准修正各语言实现。

---

## 1. eval 在 ZBRT 传输下的统一编码（基线约定）

**背景**：ZBRT v1（PROTOCOL.md §3）没有专门的 eval opcode，只有 `Execute`（kind=3）。
四语言曾出现三种映射分歧（Rust/C# 用 `argv=["eval", code]`，Python 用 `argv=[code]`，
Java 直接抛 `ValidationError` 拒绝）。统一约定如下，以 Rust 基准实现为准：

`Sandbox.eval(code, cwd=None, timeout_s=None)` 在 ZBRT 传输下编码为一轮 Execute：

| 字段 | 编码 |
|---|---|
| argv | 恰好 2 项：`["eval", code]`（argc=2；第一项是结构化操作名 `eval`，第二项是源代码原文） |
| cwd | `Some(cwd)` → cwd 标志 1 + u32 前缀字符串；`None` → cwd 标志 0（无 cwd 字节） |
| stdin | 空（u32 长度 0） |
| timeout_ms | `timeout_s=None` → `0`（无显式 deadline）；否则秒数 ×1000 |
| 响应 | 0..n 个 Output 帧（stream=0 映射 stdout，stream=1 映射 stderr）→ 恰好一个终结帧 Exit 或 Error |

结果映射与 NDJSON 传输一致：`ExecResult(exit_code, stdout, stderr, timed_out=false)`，
eval 的 `output` 语义映射为 `stdout`（UNIFIED_API.md §4）。

前置校验不变（PROTOCOL.md §2.3）：`code` 去空白后非空、≤1 MiB；`cwd` 为合法文件路径；
`timeout_s` 不得为 0。校验失败抛 `ValidationError` —— 注意：**ZBRT 下 eval 不再是
“不支持”，而是按上表正常编码发送**。

NDJSON 传输下 eval 保持 PROTOCOL.md §2.2 的 `{"action":"eval","code":...}` 不变。

## 2. 黄金向量

`conformance/eval_zbrt_vectors.json` 给出 eval-over-ZBRT 的黄金向量（Execute 帧完整 hex
与字段级期望）。request_id 统一 `000102030405060708090a0b0c0d0e0f`，与 PROTOCOL.md §4 一致。

**每个 SDK 的测试套件必须包含对应的一致性测试**（建议命名）：

- Rust：`rfb/tests/client_facade.rs::eval_zbrt_matches_shared_vector`
- Python：`tests/test_zbrt.py::test_eval_zbrt_matches_shared_vector`
- C#：`tests/Rfb.Sdk.Tests/ZbrtClientTests.cs::Eval_Zbrt_MatchesSharedVector`
- Java：`tests/src/test/java/.../ZbrtClientTest.java::evalZbrtMatchesSharedVector`

断言内容：对向量中的输入构造 Execute 帧，完整帧 hex 与 `expected_frame_hex` 逐字节相等；
同时按字段断言 argc/argv/cwd/stdin/timeout_ms，便于失败时定位。

## 3. 各语言现状（2026-09-08 修订）

| 语言 | 文件 | 状态 |
|---|---|---|
| Rust（基准） | `rfb/src/client/facade.rs` `GuestOps::eval` ZBRT 分支 | ✅ 已是基线 |
| C# | `sdk/csharp/src/Rfb.Sdk/Sandbox.cs` `Eval` | ✅ 与基线一致 |
| Python | `sdk/python/rfb_sdk/_zbrt.py` `eval` | ✅ 已改为 `argv=["eval", code]` |
| Java | `sdk/java/src/main/java/io/rfb/sdk/Sandbox.java` `eval` | ✅ 已改为按基线编码 |

各 README 的「统一 API / 已知限制」章节已同步修订为本文契约（python/java README
均描述 `argv=["eval", code]` 约定）。
