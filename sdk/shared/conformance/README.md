# sdk/shared/conformance — 共享黄金向量（ZBRT v1）

本目录存放**五个语言实现（Rust / Python / C# / Java / Node.js）共用的 wire 黄金向量**。
唯一权威文件是 [`zbrt_vectors.json`](zbrt_vectors.json)：各语言 codec 测试必须**从磁盘加载同一
文件**（逐字节编码 + 解码双向命中），不准在语言目录内复制一份；向量变更先改
`sdk/PROTOCOL.md`（§3.2 kind 表 / §4 清单），再改本 JSON。

## 1. 文件 schema（`zbrt_vectors.json`）

| 键 | 类型 | 语义 |
|---|---|---|
| `comment` | string | 用途说明，不参与断言 |
| `request_id_hex` | string | 32 位小写十六进制 = 16 字节 request_id（`000102…0f`），`frames` 里每个向量共用它；测试断言解码出的 request_id 等于它 |
| `frames` | array | 8 个帧向量，顺序固定（见 §1.1） |
| `rejects` | array | 严格解码拒绝用例（见 §1.2） |

### 1.1 `frames[]`

| 字段 | 类型 | 语义 |
|---|---|---|
| `name` | string | 向量名，固定 8 个且顺序固定：`hello` `helloack` `execute` `output` `exit` `cancel` `cancel_legacy` `error`（与 `PROTOCOL.md §4` 同清单） |
| `kind` | u8 | 帧 kind（`PROTOCOL.md §3.2`）：1/2/3/4/5/6/6/12；解码后必须等于它 |
| `hex` | string | **完整帧**（28 字节头 + payload）的小写十六进制 |

断言要求：编码方向用测试内的语义结构（如 `Hello{client:"sdk-test", caps:["execute","stream"]}`）
重新编码后逐字节等于 `hex`；解码方向解析 `hex` 后字段等于同一语义结构。

`helloack`（guest 应答）的能力集是完整 `ZBRT_V1_CAPABILITIES`（6 项，`execute` 起、`filesystem` 止）；
`cancel_legacy` 是历史 payload（reason 后无 target 字节），**只做解码断言**——编码侧不存在该形态；
`execute` 含 `argv/cwd/stdin/timeout_ms` 全字段。

### 1.2 `rejects[]`

| 字段 | 类型 | 语义 |
|---|---|---|
| `name` | string | 用例名 |
| `byte_offset` | number | 相对 **`hello` 向量完整帧**的 0-based 字节偏移 |
| `byte_value` | number | 覆盖后的字节值（0..=255） |

断言要求：构造 `hello` 向量后把 `byte_offset` 处字节改为 `byte_value`，严格 `Frame::decode`
必须失败。当前 3 例：`bad_magic`（offset 0 → 88）、`wrong_version`（4 → 2）、
`unknown_kind`（5 → 127）。

## 2. 加载约定（五语言）

统一规则：**从测试文件/测试进程位置向上查找仓库根**（即含有 `sdk/shared/conformance/` 的
祖先目录），再加载 `sdk/shared/conformance/zbrt_vectors.json`。禁止写死绝对路径，禁止复制
文件到语言树内——就地加载同一个文件才能保证五语言向量不漂移。

| 语言 | 锚点与做法 | 落点 |
|---|---|---|
| Rust | `PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sdk/shared/conformance/zbrt_vectors.json")`（`rfb/` crate 在仓库根下一级） | `rfb/tests/client_codec.rs` |
| Python | 从 `Path(__file__).resolve().parents` 逐级向上找含 `sdk/shared/conformance` 的祖先 | `tests/test_zbrt_codec.py` |
| C# | 从测试程序集/测试源文件目录向上找到含 `sdk/shared/conformance` 的仓库根再读 JSON | `tests/Rfb.Sdk.Tests/GoldenVectorTests.cs` |
| Java | 从 `user.dir` / 测试类位置向上找仓库根再读 JSON（Gson/Jackson 解析 `hex`） | `tests/src/test/java/io/rfb/sdk/ZbrtFrameCodecTest.java` |
| Node.js | 从 `import.meta.url`（回退 `process.cwd()`）向上找仓库根再读 JSON | `src/test/zbrt.test.ts` |

Rust 与 Python 已按此约定落位；其余语言补 codec 测试时沿用同一锚点规则（向上查找仓库根），
不要改成相对 cwd 的固定路径。

## 3. 新增 / 修改向量的流程

1. 先在 `sdk/PROTOCOL.md` 定义或更新 wire 语义（§3.2 kind 表），并同步 §4 清单；
2. 再更新 `zbrt_vectors.json`：新 `frames` 条目必须能**编码 + 解码双向**命中，`kind` 与
   §3.2 一致，`request_id_hex` 全帧统一；
3. 新 `rejects` 条目只表达“单字节翻转 `hello` 后严格解码必须失败”，不得用整段改写的负例
   （结构级负例留在各语言测试代码里）；
4. 五语言测试全绿前不得合并——向量是跨语言契约，任何一门语言的 codec 对不上都说明实现漂移。
