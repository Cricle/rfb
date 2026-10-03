# RFB 发布与 CI 手册（RELEASE.md）

> 面向维护者的发布流水线与 CI 完整说明。agent 可读的闭环 runbook 见
> `rfb/skills/cli-usage/SKILL.md` 与各 skill 目录；本文记录"为什么是这样"与踩坑速查。

## 1. 流水线总览

| 工作流 | 触发 | 内容 |
|---|---|---|
| `ci.yml` | 全部 PR + **所有分支推送（含 main）**（+手动） | Rust 门禁（fmt/clippy/test/doc，`--all-features`，clippy/test/doc 均带 `--locked`；含边界检查、feature 矩阵）+ SDK 全量验证（debian:12 容器内 Python / C# / Java / Node.js 四套件 build+tests） |
| `ci.yml` 的 `e2e` job | push main（+手动；PR 不跑） | KVM 真机 E2E（原独立 `e2e.yml`，2026-09-30 合并进 ci.yml——同一 checkout/缓存/并发取消）：构建 rfb-cli 与 forkd-agent rootfs → 起真实 forkd-controller → 建快照 → 跑全部 `#[ignore]` 真机测试（串行） |
| `release.yml` | push `v*.*.*` tag | 五渠道发布 + GitHub Release（见下） |

所有 workflow 的失败现场都通过 `::error::`/`::warning::` 注解带出
（check-runs annotations 公开 API 可读），因为日志下载需要 token。

### 1.1 门禁变化（2026-09 本轮）

- **main 推送跑 `ci.yml` 全部 job（含真机 e2e）**：
  Rust 门禁不再只在 PR/非 main 分支上执行。
- **`--locked` 全覆盖**：ci.yml 的 clippy/test/doc 与 msrv check、release.sh 的
  clippy/test/doc 门禁都带 `--locked`（fmt 除外），锁定 Cargo.lock 逐字节一致。
- **feature 矩阵**（ci.yml rust job 新增 step）：`cargo check --locked` 逐条跑
  rfb-sdk（no-default / +forkd / +zeroboot）与 rfb-runtime（no-default /
  +guest / +host-vsock / +firecracker / +forkd / +zeroboot）共 9 个 lean
  组合，防 `--all-features` 覆盖不到的单 feature 漏编译；任一条失败即停。
- **发布前置版本一致性门**（release.yml crates job 第一步）：tag 必须与
  六处清单一致——workspace `Cargo.toml`、`sdk/python/pyproject.toml`、
  `sdk/nodejs/package.json`、`sdk/java/pom.xml`、`Rfb.Sdk.csproj`、
  `Rfb.Cli.csproj`——外加三个 crate（cargo metadata）；任一不等即 `::error`
  列出全部不一致文件并 exit 1。契约测试 `rfb/tests/release_contract.rs`
  （零依赖、手写解析）在普通 `cargo test` 里对同一约束再兜一道底。
- **artifact 保留期**：`rfb-cli-linux-x64` 的 `retention-days` 1 → 7（便于
  失败渠道修复重跑与事后取证）。
- **NuGet push 不再把 API key 放进 argv**：改为生成临时 NuGet.Config（0600，
  凭据写 `%NUGET_KEY%` 占位由 NuGet 从环境变量展开，密钥不落盘），经
  `--configfile` 传入，run 结束（含失败路径）删除。
- **内核 pin**：e2e（ci.yml）下载的 `vmlinux-5.10.225` 下载后即用固化的 sha256
  校验（`23b3047df7dada3500d06c8012cc030b921da01e213735f7717d2166cfcf5f06`）。
- **边界脚本增强**：`scripts/check-boundary.sh` 新增依赖方向断言
  （rfb-sdk ↛ rfb-adk/rfb-ben、rfb-runtime ↛ rfb-sdk/rfb-adk、rfb-adk →
  rfb-sdk，经 `cargo metadata` 解析、工具不可用时降级为警告）；
  `scripts/release.sh` 开头先调用它。`scripts/check-tests-folder.sh` 的
  内联测试正则覆盖 `cfg(all(test,…))`/`cfg(any(test,…))` 等变体，扫描范围
  纳入 ben/ 的 src。

## 2. 发布五渠道

| 渠道 | 产物 | 发布方式 |
|---|---|---|
| crates.io | `rfb-runtime` → `rfb-sdk` → `rfb-adk`（0.0.1） | `scripts/release.sh`：门禁（fmt/test/doc）+ 按依赖序逐个 `package`+`publish`，渠道间等待索引传播；`--token` CLI 直传凭据 |
| PyPI | `rfb-sdk`（SDK 绑定） | `python -m build` + `pypa/gh-action-pypi-publish`（凭据 `PYPI_KEY`）。CLI 不做 PyPI 渠道：二进制发布不引入 Python 包装壳，CLI 走 crates.io 源码安装、GitHub Release、npm 与 NuGet |
| Maven Central | `io.github.cricle:rfb-sdk` | GPG 签名 + central-publishing-maven-plugin（自动发布，等待 published） |
| NuGet | `Rfb.Sdk`（SDK）+ `Rfb.Cli`（binary-only 包） | `dotnet pack` + `dotnet nuget push --skip-duplicate`；二进制同上来自 artifact |
| npm | `rfb-sdk`（TypeScript SDK，构建+测试后发布）+ `rfb-cli`（仅二进制包，bin 入口直连 ELF） | `npm publish`（`NODE_AUTH_TOKEN`=NPM_KEY）；凭据必须是 **granular access token**：包权限 Read and write、范围 All packages、创建时勾选 bypass 2FA（classic/automation token 会被 npm 拒绝：403 EOTP） |
| GitHub Release | `rfb-<tag>.tar.gz`、`rfb-cli-linux-x64`、`SHA256SUMS` | crates job 构建并上传 rfb-cli artifact，nuget/npm job 经 needs 下载后打包 |

主 crate 发布名是 `rfb-sdk`（crates.io 裸名 `rfb` 被 2022 年的无关项目占用）；
lib 名保持 `rfb`，`rfb-cli` 二进制不变。

## 3. 幂等语义（重跑安全）

每一渠道都先查询注册表上 `(名称, 版本)` 是否已存在，存在则跳过：

- crates.io：`scripts/release.sh` 内 `crate_published()` 查
  `crates.io/api/v1/crates/<name>/<version>`；
- PyPI：`pypi_guard` 步骤查 `pypi.org/pypi/rfb-sdk/<version>/json`；
- NuGet：`nuget_guard` 步骤查 flat-container；push 侧另有 `--skip-duplicate` 兜底；
- Maven：`maven_guard` 查 repo1.maven.org 的 POM。

因此：修完某个渠道后**重打同一个 tag 重跑**是安全的，已发布的渠道自动跳过。
版本号一旦发布成功即永久占用，失败（未发布）可原版本重试。

## 4. 一次性配置

### 4.1 Secrets（Settings → Secrets and variables → Actions）

| Secret | 内容 |
|---|---|
| `CRATES_KEY` | crates.io token |
| `PYPI_KEY` | PyPI API token（token 范围覆盖 `rfb-sdk`，或用全局 token） |
| `MAVEN_KEY` | Central 的 settings.xml `<server>` 块，`<id>` 必须为 `central`（workflow 会把任何 server id 归一为 central） |
| `GPG_PRIVATE_KEY` | armored 私钥全文（Maven Central 强制签名） |
| `GPG_PASSPHRASE` | 私钥口令 |
| `NUGET_KEY` | nuget.org API key |
| `NPM_KEY` | npm **granular access token**（Read and write + All packages + bypass 2FA；npm 2025 起拒绝 classic/automation token 的 CI 发布） |

### 4.2 Maven namespace（一次性）

groupId 是 `io.github.cricle`。首次发布前需在 https://central.sonatype.com
（GitHub 账号登录）→ Namespaces → 添加 `io.github.cricle` → 按 **GitHub 方式**
验证（自动）。`io.<自定义>` 形态的 namespace 要求拥有对应域名的 DNS
（如 `io.rfb` 需要 `rfb.com` 的 TXT 记录），无该域名就走 GitHub 路线。

### 4.3 GPG

公钥需发布到 keyserver（keys.openpgp.org / keyserver.ubuntu.com），
Central 校验签名时使用。私钥备份见仓库外的运维文档。

## 5. 兼容性矩阵

| 组件 | 支持范围 | 说明 |
|---|---|---|
| Rust crate | Rust 1.93+（edition 2021，`rust-version` 已在各 Cargo.toml 声明） | CI 用 stable 构建；MSRV 由 Cargo.lock 依赖下限决定 |
| Java SDK（io.github.cricle:rfb-sdk） | **Java 8+**（8/11/16/17/21…） | 内部值类型为 Java 8 兼容手写类（原 record 已降级）；HTTP 层用 HttpURLConnection（无 java.net.http 依赖） |
| C# SDK（Rfb.Sdk） | netstandard2.1 / net8.0 | System.Text.Json 经条件包引用（net8 内置）；PolySharp（编译期，PrivateAssets）提供 Range/Index 等语法糖；ns2.0 因缺 Span/BinaryPrimitives 且拒绝引入 System.Memory 依赖链而不再目标 |
| Python SDK（rfb-sdk） | Python 3.9+（`requires-python = ">=3.9"`） | 纯 stdlib；tests 用 unittest |
| rfb-cli 二进制 | linux-x64（glibc，ubuntu-24.04 构建） | crates.io 源码安装无此限制；分发渠道：crates.io、GitHub Release、npm（`rfb-cli`，bin 直连 ELF）、NuGet（`Rfb.Cli`，tools 直放二进制）——全部为纯二进制/清单，无包装壳 |

## 6. 失败速查表（真实踩坑记录）

| 症状 | 根因 | 修复 |
|---|---|---|
| 容器 job 的 run 步骤 ~10s 秒退 exit 2，无日志 | debian:12 没有 `which`，runner 探测 bash 失败回退 sh（dash），`set -o pipefail` 非法 | 容器内步骤显式 `shell: bash` |
| dotnet 首次启动 FailFast / rc=134 | debian:12 容器缺 libicu | apt 装 `libicu72`（bookworm）/ `libicu76`（trixie），脚本按版本回退 |
| 快照就绪等待 300s 超时但快照早已 ready | `python3 -c` 内嵌代码继承 YAML 块缩进 → IndentationError，每次轮询静默失败 | 判定脚本用 heredoc 顶格落盘（内容行与块同缩进，YAML 剥离后顶格） |
| E2E step 11 秒死 exit 1，`$TAG: unbound variable` | GITHUB_ENV 写入的变量不传播（实测） | TAG 按 `ci-e2e-$GITHUB_RUN_ID` 在使用处重算 |
| 测试断言 `IsExternalInit` 缺失 | `init` 访问器 / record struct 需要该编译器注入类型 | 仓库内置 `Internal/IsExternalInit.cs` polyfill（ns2.1 目标） |
| NS2.0 报 WriteAsync/ConnectAsync/GetBytes/`^` 重载缺失 | Memory/ValueTask/CT 重载是 .NET 5+；`^` 需 System.Index | `#if NET5_0_OR_GREATER` 条件编译 + 经典重载回退 |
| CI 的 E2E 全绿但 agent_contract 17 个测试失败 | runner 的 `/tmp` 是 ext4：删除重建可复用同 inode，身份 pinning 漏检 | EndpointIdentity 增加 birth time |
| Java 8 目标编译失败：records / java.net.http / writeBytes / URLEncoder(Charset) | SDK 源码用了 JDK16+/11+/10+ 特性 | record→手写值类；HttpClient→HttpURLConnection；writeBytes→write(b,0,len)；encode(Str,Charset)→encode(Str,"UTF-8") |
| 整仓 259 文件级 diff / shell 脚本在 Linux 崩 | Windows 检出的 CRLF 被整仓提交 | `.gitattributes`（`* text=auto eol=lf`）+ renormalize；提交前检查 diff 规模 |
| crates 发布 "no token found" | runner 的 cargo 未识别 `CARGO_REGISTRIES_CRATES_IO_TOKEN` 环境变量 | `cargo publish --token <token>` CLI 直传 |
| Maven "Namespace is not allowed" | groupId 的 namespace 未在 Central portal 验证 | 见 §4.2；`io.<自定义>` 需域名 DNS，`io.github.<user>` 用 GitHub 账号 |
| Maven "Project URL/License/SCM/Developers is missing" | Central 元数据硬性校验 | POM 补 url/licenses/scm/developers |

## 7. 本地开发流

1. 本地是 GitHub 仓库的浅克隆（`--depth 1`）；**所有 push 只在远程构建服务器**
   `/root/github/rfb` 执行（该机 git 身份为发布账号）。
2. 修改后：`git diff origin/main --binary > fix.patch`（新增文件先 `git add -N`
   或 `git add -A` 后用 `git diff --cached`）。
3. `scp fix.patch server:/tmp/` → 远端 `git apply --check && git apply` →
   远端 commit + push。
4. 注意本地检出的 CRLF：diff 规模异常大（数百文件）= 行尾问题，先 renormalize。
5. 构建产物目录（`sdk/java/target/` 等）已被 .gitignore 覆盖，`git add -A`
   前确认 `git status --short` 无 target 泄漏。
