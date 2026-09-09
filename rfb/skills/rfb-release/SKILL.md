---
name: rfb-release
version: 1.0.0
description: rfb 发布验证闭环：打 tag v0.0.1 触发 Release（crates.io/PyPI/Maven Central/NuGet 四渠道）→ 监控 CI run → 失败拉日志定位 → 修复重试直到全绿。当用户要验证发布链路、打 tag 发版、或排查 Release workflow 失败时使用。
---

# rfb 发布验证闭环（tag → CI → 日志 → 修复 → 重试直到全绿）

## 适用场景

- 打 tag（如 `v0.0.1`）验证 crates.io / PyPI / Maven Central / NuGet 四渠道发布。
- Release workflow 失败后的定位与重试。

## 前置事实（务必先核对，不要凭记忆）

1. **版本一致性**：tag 必须等于 `v<Cargo.toml workspace version>`。当前工作区
   版本见根 `Cargo.toml` 的 `[workspace.package] version`；`sdk/python/pyproject.toml`、
   `sdk/java/pom.xml`、`sdk/csharp/src/Rfb.Sdk/Rfb.Sdk.csproj` 的版本号须手动同步。
2. **触发**：`.github/workflows/release.yml` 由 `push: tags: ['v*.*.*']` 触发；
   main 推送跑 e2e.yml（真机 E2E），其他分支/PR 跑 ci.yml（fmt/clippy/test/doc）。
3. **secrets**（在仓库 Settings → Secrets and variables → Actions 配置）：
   - `CRATES_KEY`：crates.io token。发布链 rfb-runtime → rfb-sdk → rfb-rig
     （主 crate 发布名 `rfb-sdk`——crates.io 裸名 `rfb` 被无关项目占用，
     lib 名仍是 `rfb`，代码引用与 `rfb-cli` 二进制不变）。
   - `PYPI_KEY`：PyPI API token（包名 `rfb-sdk`）。
   - `MAVEN_KEY`：Maven Central 的 settings.xml `<server>` 块，`<id>` 必须为
     `central`（central-publishing-maven-plugin 的 publishingServerId）。
   - `GPG_PRIVATE_KEY` + `GPG_PASSPHRASE`：Maven Central 强制 GPG 签名。
   - `NUGET_KEY`：nuget.org API key（包名 `Rfb.Sdk`）。
4. **crates.io / PyPI / nuget.org 的首次发布不可撤销**：版本号一旦发布成功就
   永久占用，重试只能递增版本或换 tag。修 CI 时先用 dry-run（见"步骤 0"）。

## 流程

### 步骤 0：推送前本地/远程预检

- `bash scripts/release.sh --check`：fmt/test/doc 门禁 + `cargo publish
  --dry-run`（不出网，不消耗版本号）。
- 版本号四处一致（Cargo workspace / pyproject / pom / csproj）。

### 步骤 1：提交并打 tag

```bash
# 所有 git 操作在远程服务器（或 CI 侧），不要在本地工作目录跑 git push
git add -A && git commit -m "release: v0.0.1"
git tag v0.0.1 && git push origin main v0.0.1
```

### 步骤 2：监控 CI run

```bash
# 无 token 也可用公开 API（仓库为 public）
curl -s "https://api.github.com/repos/Cricle/rfb/actions/runs?event=push&per_page=5" \
  | python3 -c 'import json,sys; [print(r["name"], r["head_branch"], r["status"], r["conclusion"], r["html_url"]) for r in json.load(sys.stdin)["workflow_runs"]]'
```

轮询直到 `status == completed`；`conclusion == success` 即完成，`failure` 进步骤 3。

### 步骤 3：失败拉日志定位

```bash
RUN_ID=<失败 run 的 id>
curl -s "https://api.github.com/repos/Cricle/rfb/actions/runs/$RUN_ID/jobs" \
  | python3 -c 'import json,sys; [print(j["name"], j["conclusion"]) for j in json.load(sys.stdin)["jobs"]]'
# 下载日志（public 仓库无 token 可直接 GET）
curl -sL "https://api.github.com/repos/Cricle/rfb/actions/runs/$RUN_ID/logs" -o logs.zip
```

只看失败 job 的失败 step 日志；常见错误见"失败速查"。

### 步骤 4：修复并重试

1. 修复代码/配置 → 重新 commit + push 到 main（先让 e2e 过）。
2. **tag 重打**：tag 指向旧 commit 时必须先删再打：
   ```bash
   git push origin :refs/tags/v0.0.1 && git tag -d v0.0.1
   git tag v0.0.1 && git push origin v0.0.1
   ```
3. 回到步骤 2，循环直到全绿。

## 失败速查

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| crates job: `tag v0.0.1 != v<ver>` | workspace 版本与 tag 不一致 | 改 Cargo.toml（workspace.package.version）并同步 4 处版本 |
| crates job: `already exists` | 版本已发布过 | crates.io 不可覆盖：递增版本号重发 |
| pypi job: 400/403 | PYPI_KEY 无效或包名冲突 | 核对 token；包名 `rfb-sdk` 已占用时换名 |
| maven job: 401 Unauthorized | MAVEN_KEY 的 `<id>` 不是 `central` | settings.xml server id 必须为 `central` |
| maven job: `Missing Signature` | 未提供 GPG | 配 `GPG_PRIVATE_KEY`+`GPG_PASSPHRASE` |
| maven job: groupId 未验证 | `io.rfb` 命名空间未在 Central Portal 验证 | 到 portal.sonatype.com 完成验证 |
| nuget job: 409/conflict | 版本已存在或元数据缺失 | 递增版本；核对 csproj 包元数据 |

## 硬约束

- 本地（Windows 工作目录）绝不执行 git 写操作；commit/push 一律在远程。
- crates.io/PyPI/NuGet/Central 发布成功后版本永久占用——任何"重发"都要换版本号。
- secrets 只能由用户在 GitHub 仓库设置里配置，不要把凭据写进仓库文件（仓库公开）。
