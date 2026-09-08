#!/usr/bin/env bash
# =============================================================================
# RFB SDK 全新 Debian 12 环境一键 E2E 验证脚本
#
# 目的：在一台"干净"的 Debian 12（WSL2 发行版或 CI 容器）里，从零安装工具链
# 并把三个 SDK 测试套件全部跑绿：
#   A) Python  : python3 -m unittest discover -s tests -v
#   B) C#      : dotnet test（先清理 bin/obj 陈旧产物）
#   C) Java    : mvn test（surefire）
# 依赖安装（apt + dot.net 官方安装脚本）与测试执行全部内聚在本脚本内，
# 不依赖环境里预装的任何 SDK 工具链。脚本不含任何机密。
#
# 用法 A —— WSL2（Windows 宿主，仓库位于 /mnt/c/...）：
#   # Git Bash 下必须设 MSYS_NO_PATHCONV=1，否则 /mnt/... 等绝对路径会被
#   # Git Bash 改写成 C:/Program Files/Git/... 导致命令失败
#   MSYS_NO_PATHCONV=1 wsl.exe -d rfb-e2e-debian -u root -- \
#     bash /mnt/c/workplace/mm/monitor/rfb/sdk/e2e/debian-e2e.sh
#
# 用法 B —— CI/容器（debian:12 镜像，仓库检出到工作目录）：
#   docker run --rm -v "$PWD":/repo -w /repo debian:12 \
#     bash sdk/e2e/debian-e2e.sh
#   # GitHub Actions 用法见 .github/workflows/sdk-e2e.yml（脚本会自动识别
#   # 非 WSL 环境，走同一套 apt + dotnet-install 依赖流程）
#
# 环境变量：
#   E2E_SKIP_PYTHON=1 / E2E_SKIP_DOTNET=1 / E2E_SKIP_JAVA=1  跳过对应套件
#   E2E_SKIP_APT=1                                           跳过 apt 安装（依赖已备好时）
# 注意（WSL2）：wsl.exe 不会把宿主环境变量转发进发行版，例如
#   MSYS_NO_PATHCONV=1 E2E_SKIP_DOTNET=1 wsl.exe -d ... -- bash script.sh
# 中的 E2E_SKIP_DOTNET 不会生效。请在发行版内部设置这些变量，或用
#   wsl.exe ... -- env E2E_SKIP_DOTNET=1 bash /mnt/c/.../debian-e2e.sh
# 的方式注入（env 在发行版内执行，可以生效）。
#   E2E_OUT_DIR=...          日志/汇总输出目录（默认 mktemp，退出时由 trap 清理；CI 传此变量以便上传）
#   DOTNET_CHANNEL=...       dotnet SDK 大版本（默认 8.0）
#   DOTNET_INSTALL_DIR=...   dotnet 安装目录（默认 $HOME/.dotnet）
#
# 退出码：0 = 全部启用的套件通过；1 = 有启用的套件失败；12 = 基础工具缺失。
# =============================================================================
set -euo pipefail

# REPO_ROOT 从脚本自身位置解析：脚本位于 <repo>/sdk/e2e/，上溯两级即仓库根。
# 因此无论仓库挂在 /mnt/c/...（WSL）还是检出到 $PWD（CI 容器）都能正确定位。
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SDK_DIR="$REPO_ROOT/sdk"

# 环境判定：WSL 下 /proc/version 含 microsoft（仅用于日志提示，不影响逻辑）
IS_WSL=0
if grep -qi microsoft /proc/version 2>/dev/null; then IS_WSL=1; fi

log()  { printf '[e2e] %s\n' "$*"; }
die12() { printf '[e2e] 错误：%s\n' "$*" >&2; exit 12; }

# ---- 输出目录与 trap 清理 --------------------------------------------------
# 默认输出到系统临时目录（本地一次性使用，退出即清）；CI 用 E2E_OUT_DIR 指向
# 工作区内的固定路径，退出时保留以便上传 artifact。
OUT_DIR="${E2E_OUT_DIR:-$(mktemp -d /tmp/rfb-sdk-e2e.XXXXXX)}"
mkdir -p "$OUT_DIR"
TMP_DIRS=()
cleanup() {
  for d in "${TMP_DIRS[@]:-}"; do
    rm -rf "$d" 2>/dev/null || true
  done
  # 只有使用默认临时目录时才清理；显式指定的 E2E_OUT_DIR 保留给调用方
  if [ -z "${E2E_OUT_DIR:-}" ]; then
    rm -rf "$OUT_DIR" 2>/dev/null || true
  fi
}
trap cleanup EXIT

log "仓库根：$REPO_ROOT (WSL=$IS_WSL)"
log "输出目录：$OUT_DIR"

# ---- 0. 基础工具门禁：非 Debian 系 / 无 apt-get 直接退出 12 ----------------
command -v apt-get >/dev/null 2>&1 || die12 "未找到 apt-get：本脚本仅支持 Debian 系环境"
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  command -v sudo >/dev/null 2>&1 || die12 "当前非 root 且没有 sudo，无法安装系统依赖"
  SUDO="sudo"
fi

# ---- 1. 系统依赖（apt）-----------------------------------------------------
# python3 + curl + ca-certificates：Python 套件与后续 dotnet 安装脚本所需；
# openjdk-17-jdk-headless + maven：Java 套件。dotnet 不在 apt 里，下一步单独装。
if [ "${E2E_SKIP_APT:-0}" != "1" ]; then
  log "安装 apt 依赖（python3 / curl / ca-certificates / openjdk-17-jdk-headless / maven）..."
  export DEBIAN_FRONTEND=noninteractive
  $SUDO apt-get update -qq
  $SUDO apt-get install -y -qq --no-install-recommends \
    python3 curl ca-certificates openjdk-17-jdk-headless maven
else
  log "E2E_SKIP_APT=1：跳过 apt 依赖安装"
fi

# ---- 2. dotnet SDK ----------------------------------------------------------
# 优先复用环境里已有的 dotnet（PATH 上或 DOTNET_ROOT 下）；都没有才通过
# dot.net 官方安装脚本装到用户目录，避免依赖 apt 源里的 microsoft 源配置。
ensure_dotnet() {
  if command -v dotnet >/dev/null 2>&1; then
    log "已检测到 dotnet：$(dotnet --version)"
    return 0
  fi
  if [ -n "${DOTNET_ROOT:-}" ] && [ -x "${DOTNET_ROOT}/dotnet" ]; then
    export PATH="${DOTNET_ROOT}:$PATH"
    log "使用 DOTNET_ROOT=${DOTNET_ROOT} 下的 dotnet：$("${DOTNET_ROOT}/dotnet" --version)"
    return 0
  fi
  local install_dir="${DOTNET_INSTALL_DIR:-$HOME/.dotnet}"
  local tmp
  tmp="$(mktemp -d)"
  TMP_DIRS+=("$tmp")
  command -v curl >/dev/null 2>&1 || die12 "未找到 curl，无法下载 dotnet 安装脚本"
  log "通过 dot.net 官方脚本安装 .NET SDK（channel=${DOTNET_CHANNEL:-8.0}）→ ${install_dir} ..."
  curl -fsSL https://dot.net/v1/dotnet-install.sh -o "$tmp/dotnet-install.sh"
  bash "$tmp/dotnet-install.sh" --channel "${DOTNET_CHANNEL:-8.0}" --install-dir "$install_dir"
  export DOTNET_ROOT="$install_dir"
  export PATH="${DOTNET_ROOT}:$PATH"
  export DOTNET_CLI_TELEMETRY_OPTOUT=1
  dotnet --version
}

# ---- 3. 三个套件 ------------------------------------------------------------
PASS=0
FAIL=0
SKIP=0
SUMMARY=()

# 套件 A：Python SDK（unittest discover）
if [ "${E2E_SKIP_PYTHON:-0}" = "1" ]; then
  SUMMARY+=("python : SKIP (E2E_SKIP_PYTHON=1)")
  SKIP=$((SKIP + 1))
else
  command -v python3 >/dev/null 2>&1 || die12 "python3 缺失，无法运行 Python 套件"
  log "运行 Python SDK 测试（python3 -m unittest discover -s tests -v）..."
  if (cd "$SDK_DIR/python" && python3 -m unittest discover -s tests -v) \
      2>&1 | tee "$OUT_DIR/python.log"; then
    SUMMARY+=("python : PASS")
    PASS=$((PASS + 1))
  else
    SUMMARY+=("python : FAIL（详见 $OUT_DIR/python.log）")
    FAIL=$((FAIL + 1))
  fi
fi

# 套件 B：C# SDK（dotnet test）
# 先清理 bin/obj：跨环境复用工作区（尤其 WSL drvfs）时陈旧产物会造成假失败。
if [ "${E2E_SKIP_DOTNET:-0}" = "1" ]; then
  SUMMARY+=("dotnet : SKIP (E2E_SKIP_DOTNET=1)")
  SKIP=$((SKIP + 1))
else
  ensure_dotnet
  log "清理 C# bin/obj 陈旧产物..."
  find "$SDK_DIR/csharp" -type d \( -name bin -o -name obj \) -prune \
    -exec rm -rf {} + 2>/dev/null || true
  log "运行 C# SDK 测试（dotnet test tests/Rfb.Sdk.Tests/Rfb.Sdk.Tests.csproj）..."
  # 注意：.NET 8 SDK 无法解析 .slnx 解决方案格式（MSB1006/MSB4068），必须直接
  # 指向测试工程 csproj，而不是 dotnet test 整个解决方案。
  if (cd "$SDK_DIR/csharp" && dotnet test tests/Rfb.Sdk.Tests/Rfb.Sdk.Tests.csproj \
      --logger "trx;LogFileName=e2e-csharp.trx") \
      2>&1 | tee "$OUT_DIR/dotnet.log"; then
    SUMMARY+=("dotnet : PASS")
    PASS=$((PASS + 1))
  else
    SUMMARY+=("dotnet : FAIL（详见 $OUT_DIR/dotnet.log）")
    FAIL=$((FAIL + 1))
  fi
fi

# 套件 C：Java SDK（mvn test，surefire 报告落在 target/surefire-reports）
if [ "${E2E_SKIP_JAVA:-0}" = "1" ]; then
  SUMMARY+=("java   : SKIP (E2E_SKIP_JAVA=1)")
  SKIP=$((SKIP + 1))
else
  command -v mvn >/dev/null 2>&1 || die12 "mvn 缺失，无法运行 Java 套件"
  command -v java >/dev/null 2>&1 || die12 "java 缺失，无法运行 Java 套件"
  log "运行 Java SDK 测试（mvn install -DskipTests + tests 模块 mvn test）..."
  # 测试已拆分到独立 maven 模块 sdk/java/tests：先在父模块 sdk/java 上
  # install -DskipTests 把主 SDK 构件装入本地仓库，再进入 tests 模块跑 surefire。
  # 注意：只对 sdk/java 跑 mvn test 会因该模块不再含测试而“空转 PASS”。
  if { (cd "$SDK_DIR/java" && mvn -B -ntp install -DskipTests) \
      && (cd "$SDK_DIR/java/tests" && mvn -B -ntp test); } \
      2>&1 | tee "$OUT_DIR/maven.log"; then
    # surefire 结果计数：0 个测试执行视为失败，防止“空转 PASS”。
    SUREFIRE_DIR="$SDK_DIR/java/tests/target/surefire-reports"
    TESTS_RUN=0
    if [ -d "$SUREFIRE_DIR" ]; then
      TESTS_RUN=$(grep -hoE 'Tests run: [0-9]+' "$SUREFIRE_DIR"/*.txt 2>/dev/null \
        | grep -oE '[0-9]+' | awk '{s += $1} END {print s + 0}')
    fi
    if [ "${TESTS_RUN:-0}" -gt 0 ]; then
      log "Java surefire 实际执行测试数：$TESTS_RUN"
      SUMMARY+=("java   : PASS ($TESTS_RUN tests)")
      PASS=$((PASS + 1))
    else
      log "Java surefire 未执行任何测试（0 tests），判为失败"
      SUMMARY+=("java   : FAIL（surefire 0 tests，详见 $OUT_DIR/maven.log）")
      FAIL=$((FAIL + 1))
    fi
  else
    SUMMARY+=("java   : FAIL（详见 $OUT_DIR/maven.log）")
    FAIL=$((FAIL + 1))
  fi
fi

# ---- 4. 汇总与退出码 --------------------------------------------------------
{
  echo "========== RFB SDK Debian E2E 汇总 =========="
  for line in "${SUMMARY[@]}"; do
    echo "  $line"
  done
  echo "结果：$PASS/3 套件通过（失败 $FAIL，跳过 $SKIP）"
  echo "输出目录：$OUT_DIR"
} | tee "$OUT_DIR/summary.txt"

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
exit 0
