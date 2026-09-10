# RFB SDK 全新 Debian 环境 E2E

## 目的

在一台**全新**的 Debian 12 环境里验证三个 SDK 测试套件完整有效：从零安装工具链
（python3 / .NET 8 / openjdk-17 + maven），然后跑齐：

- Python SDK：`python3 -m unittest discover -s tests -v`
- C# SDK：`dotnet test`（先清理 bin/obj 陈旧产物）
- Java SDK：`mvn test`（surefire）

这样能捕捉"在开发机上能跑、在干净环境缺依赖/缺文件"类问题，保证 SDK 交付物
自带完整可验证的测试资产。

## 覆盖矩阵

| 套件 | 目录 | 命令 | 验证记录（rfb-e2e-debian，2026-09） |
|------|------|------|--------------------------------------|
| Python | `sdk/python` | `python3 -m unittest discover -s tests -v` | 119 个用例全绿 |
| C# | `sdk/csharp` | `dotnet test`（.NET 8 SDK 8.0.424） | 构建与测试全绿 |
| Java | `sdk/java` | `mvn test`（openjdk-17 + maven） | 8 个测试类 101 个用例，0 失败 0 错误 0 跳过 |
| Rust | 不在本脚本范围 | — | 由 cargo 门禁（fmt/clippy/test）与 `rfb e2e.yml` 覆盖 |

## 本地用法（Windows + WSL2）

### 1) 准备一个全新的 Debian 12 发行版（一次性）

有 Docker 时导出 rootfs 并导入 WSL：

```powershell
docker pull debian:12
docker export (docker create debian:12) -o debian12-rootfs.tar
wsl.exe --import rfb-e2e-debian D:\wsl\rfb-e2e-debian .\debian12-rootfs.tar --version 2
```

没有 Docker 时用 **podman** 等价替代（导出 debian:12 rootfs，其余步骤相同）：

```bash
podman pull debian:12
podman export $(podman create debian:12) -o debian12-rootfs.tar
```

### 2) 运行脚本

仓库在 Windows 侧挂载为 `/mnt/c/workplace/mm/monitor/rfb`，直接以 root 运行
（导入的 rootfs 无 sudo 用户，root 直登）：

```bash
# Git Bash 下必须设 MSYS_NO_PATHCONV=1，否则 /mnt/... 等绝对路径会被
# Git Bash 改写成 C:/Program Files/Git/... 导致命令失败
MSYS_NO_PATHCONV=1 wsl.exe -d rfb-e2e-debian -u root -- \
  bash /mnt/c/workplace/mm/monitor/rfb/sdk/e2e/debian-e2e.sh
```

注意：

- 首次运行会通过 apt 安装 python3 / curl / ca-certificates / openjdk-17-jdk-headless /
  maven，并通过 dot.net 官方脚本安装 .NET 8 SDK，全程无需手动干预。
- WSL 内仓库位于 `/mnt/c`（drvfs），I/O 偏慢属正常现象，不影响功能。

### 只跑部分套件 / 复用已装依赖

```bash
E2E_SKIP_PYTHON=1 E2E_SKIP_DOTNET=1 E2E_SKIP_APT=1  # 例如只跑 Java
```

| 环境变量 | 作用 |
|----------|------|
| `E2E_SKIP_PYTHON=1` | 跳过 Python 套件 |
| `E2E_SKIP_DOTNET=1` | 跳过 C# 套件 |
| `E2E_SKIP_JAVA=1` | 跳过 Java 套件 |
| `E2E_SKIP_APT=1` | 跳过 apt 依赖安装（依赖已备好时） |
| `E2E_OUT_DIR=...` | 日志/汇总输出目录（默认系统临时目录，退出即清） |
| `DOTNET_CHANNEL` | dotnet SDK 大版本（默认 8.0） |
| `DOTNET_INSTALL_DIR` | dotnet 安装目录（默认 `$HOME/.dotnet`） |

### 退出码

| 退出码 | 含义 |
|--------|------|
| 0 | 全部启用的套件通过 |
| 1 | 有启用的套件失败（逐套件日志在输出目录 `python.log` / `dotnet.log` / `maven.log`） |
| 12 | 基础工具缺失（非 Debian 系 / 无 apt-get / 非 root 且无 sudo / curl 缺失等） |

## CI 行为

`.github/workflows/sdk-e2e.yml`（`RFB SDK Debian E2E`）：

- 触发：`workflow_dispatch`；push 到 `main` 且改动 `sdk/**` 或工作流文件本身；
  每周一 02:30 UTC 的 weekly schedule。
- 运行环境：`ubuntu-latest` 上的 `container: debian:12`，与本地 WSL 验证环境同源，
  无需 WSL —— 脚本自动识别非 WSL 环境，走同一套 apt + dotnet-install 依赖流程。
- 步骤：装 git → checkout → `bash sdk/e2e/debian-e2e.sh` → `always()` 上传
  汇总日志 + C# trx + Java surefire XML artifact。
- 约束与仓库其他工作流一致：`permissions: contents: read`、并发组排队、零 secret。

## Rust 为什么不在覆盖矩阵里

Rust 侧（`rfb` / `rfb-runtime` 等 crate）由仓库强制的 cargo 门禁
（`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
`cargo test --workspace`）以及真机 E2E 工作流 `.github/workflows/e2e.yml` 覆盖，
本脚本只聚焦三个 SDK 的"全新环境可验证性"。
