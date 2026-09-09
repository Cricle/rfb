#!/usr/bin/env bash
# run-real.sh —— 真机 forkd E2E 本地驱动（tests/forkd_real_e2e.rs 的注释所
# 承诺的入口）。与 .github/workflows/e2e.yml 走同一条序列：环境检查 →
# /dev/kvm 检查 → 构建（可用环境变量跳过）→ 建 TAP → 启动 forkd-controller
# → snapshot-create → 等 ready+bootable → 以 RFB_REAL_E2E=1 串行跑 #[ignore]
# 测试 → trap 清理。适用 WSL2 / 原生 Linux。
#
# 用法（在仓库任意位置）：
#   bash rfb/tests/run-real.sh                 # 全流程（含构建）
#   RFB_E2E_SKIP_BUILD=1 bash rfb/tests/run-real.sh   # 跳过构建（已建好）
#   RFB_E2E_TAG=mytag bash rfb/tests/run-real.sh      # 指定快照 tag
#   RFB_E2E_KEEP=1 bash rfb/tests/run-real.sh         # 失败后保留现场调试
#
# 退出码（与 docs 的退出码表一致）：
#   0   成功
#   1   运行/清理阶段失败（先完成清理再退出）
#   12  前置条件缺失（非 Linux、无 /dev/kvm、缺工具、cni0 地址冲突等）
#
# 安全边界：本脚本不读取、不传入任何 token/API key（不设 FORKD_TOKEN）。

set -euo pipefail

log() { printf '[run-real] %s\n' "$*"; }
die12() { printf '[run-real] PREREQ-MISSING: %s\n' "$*" >&2; exit 12; }

# ---- 定位工作区：脚本位于 rfb 工作区内的 crate tests 目录（rfb/rfb/tests）。
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "$SCRIPT_DIR/../.." && pwd)"
CRATE_DIR="$WORKSPACE/rfb"
CLI_BIN="$WORKSPACE/target/debug/rfb-cli"
FORKD_BIN_DEFAULT="$WORKSPACE/resx/forkd/forkd"
CONTROLLER_BIN="$WORKSPACE/resx/forkd/forkd-controller"
KERNEL_BIN="$WORKSPACE/resx/kernel/vmlinux-arcbox-0.0.24"
ROOTFS_DIR="$WORKSPACE/resx/rootfs"
ROOTFS_IMG="$ROOTFS_DIR/forkd-agent.ext4"
TAG="${RFB_E2E_TAG:-rfb-e2e-$$-$(date +%s)}"
RUN_DIR="${RFB_E2E_RUN_DIR:-/tmp/rfb-e2e-$$}"
SNAPSHOT_ROOT="${HOME}/.local/share/forkd/snapshots"
FORKD_URL="http://127.0.0.1:8889"
TAP_DEV="forkd-tap0"

# ---- 清理：任何退出路径都先做 teardown（幂等，尽力而为）。
TAP_CREATED_BY_US=0
CONTROLLER_OURS=0
cleanup() {
  local status=$?
  log "cleanup 开始（exit status=$status）"
  # 只清理本轮创建的资源：不 pkill、不碰他人 controller / 既有 TAP
  #（real-machine-runbook.md 的安全边界）。
  if [ "$CONTROLLER_OURS" -eq 1 ] && [ -x "$CLI_BIN" ]; then
    FORKD_ROOTFS="$ROOTFS_IMG" "$CLI_BIN" forkd snapshot-delete \
      --tag "$TAG" --force --json >/dev/null 2>&1 \
      || log "snapshot-delete $TAG 跳过/失败（可能未创建）"
  fi
  if [ "$CONTROLLER_OURS" -eq 1 ] && [ -f "$RUN_DIR/controller/pid" ]; then
    kill "$(cat "$RUN_DIR/controller/pid")" 2>/dev/null || true
  fi
  if [ "$TAP_CREATED_BY_US" -eq 1 ]; then
    sudo ip link set "$TAP_DEV" down 2>/dev/null || true
    sudo ip tuntap del dev "$TAP_DEV" mode tap 2>/dev/null || true
  fi
  if [ "${RFB_E2E_KEEP:-0}" = "1" ]; then
    log "RFB_E2E_KEEP=1：保留现场 $RUN_DIR（含 controller audit 日志）供排查"
  else
    rm -rf "$RUN_DIR"
  fi
  log "cleanup 完成"
}
trap cleanup EXIT INT TERM

# ---- 第 0 步：前置条件检查（缺失一律 exit 12）。
command -v uname >/dev/null 2>&1 || die12 "uname 不可用"
[ "$(uname -s)" = "Linux" ] || die12 "仅在 WSL2/原生 Linux 上运行（当前：$(uname -s)）"
[ "$(uname -m)" = "x86_64" ] || die12 "需要 x86_64 架构（当前：$(uname -m)）"

if [ ! -e /dev/kvm ]; then
  die12 "/dev/kvm 不存在：启用嵌套虚拟化（WSL2 见 troubleshooting）后再试"
fi
if ! [ -w /dev/kvm ]; then
  log "/dev/kvm 对当前用户不可写，尝试 sudo 放宽权限"
  sudo chmod 666 /dev/kvm || die12 "/dev/kvm 不可读写且无法提权（把用户加入 kvm 组）"
fi
[ -w /dev/kvm ] || die12 "/dev/kvm 仍不可读写"

MISSING=""
for tool in cargo musl-gcc mke2fs debugfs readelf objdump curl python3 ip; do
  command -v "$tool" >/dev/null 2>&1 || MISSING="$MISSING $tool"
done
if [ -n "$MISSING" ]; then
  die12 "缺少工具:$MISSING（apt 装 musl-tools/e2fsprogs/binutils）"
fi

# firecracker：PATH 缺失时从 resx 安装 v1.12.1 到 /usr/local/bin（与 CI 的
# e2e.yml 一致；版本门禁要求 v1.12.x，v1.16.1 会被拒绝）。
if ! command -v firecracker >/dev/null 2>&1; then
  if [ -f "$WORKSPACE/resx/firecracker/firecracker-v1.12.1" ]; then
    log "firecracker 不在 PATH，从 resx 安装 v1.12.1 到 /usr/local/bin"
    sudo install -m 755 "$WORKSPACE/resx/firecracker/firecracker-v1.12.1" \
      /usr/local/bin/firecracker
    firecracker --version
  else
    die12 "缺少 firecracker（且 resx/firecracker/firecracker-v1.12.1 不存在）"
  fi
fi

if ip addr show cni0 2>/dev/null | grep -q "10\.42\.0\.1/24"; then
  die12 "cni0 已占用 10.42.0.1/24：按 requirements/RFB/0.1.0/real-machine-runbook.md 临时挪址后再跑"
fi

[ -f "$FORKD_BIN_DEFAULT" ] || die12 "缺少 $FORKD_BIN_DEFAULT（forkd 官方二进制）"
[ -f "$CONTROLLER_BIN" ] || die12 "缺少 $CONTROLLER_BIN（forkd-controller）"
[ -f "$KERNEL_BIN" ] || die12 "缺少 $KERNEL_BIN（guest kernel）"

# ---- 构建：rfb-cli（debug，与 cargo test 同 profile）→ musl 静态 runtime
# → forkd-agent rootfs。RFB_E2E_SKIP_BUILD=1 可跳过（产物已存在时）。
mkdir -p "$ROOTFS_DIR"
if [ "${RFB_E2E_SKIP_BUILD:-0}" != "1" ]; then
  log "cargo build -p rfb --features cli"
  (cd "$WORKSPACE" && cargo build -p rfb --features cli)
  log "rfb-cli image build-static（musl 静态 rfb-runtime）"
  "$CLI_BIN" image build-static \
    --root "$WORKSPACE" --target x86_64-unknown-linux-musl --package rfb-runtime
  log "rfb-cli image build-rootfs --mode forkd-agent"
  "$CLI_BIN" image build-rootfs \
    "$WORKSPACE/target/x86_64-unknown-linux-musl/release/rfb-runtime" \
    "$ROOTFS_IMG" --mode forkd-agent --force
else
  log "RFB_E2E_SKIP_BUILD=1：跳过构建"
fi
[ -x "$CLI_BIN" ] || die12 "rfb-cli 不存在：$CLI_BIN（先去掉 RFB_E2E_SKIP_BUILD）"
[ -f "$ROOTFS_IMG" ] || die12 "rootfs 缺失：$ROOTFS_IMG"

# ---- 临时 TAP：已存在且 UP 则复用，否则创建（需 root，结束后删除）。
if ip -o link show "$TAP_DEV" >/dev/null 2>&1; then
  log "TAP $TAP_DEV 已存在，复用"
else
  log "创建 TAP $TAP_DEV（10.42.0.1/24）"
  sudo ip tuntap add dev "$TAP_DEV" mode tap
  sudo ip addr add 10.42.0.1/24 dev "$TAP_DEV"
  sudo ip link set "$TAP_DEV" up
  TAP_CREATED_BY_US=1
fi

# ---- forkd-controller：独立 state/audit/pid，snapshot root 指向默认快照目录
# （provenance 测试会从 $HOME/.local/share/forkd/snapshots/<tag> 读取产物）。
mkdir -p "$RUN_DIR/controller/state" "$SNAPSHOT_ROOT"
log "启动 forkd-controller（state=$RUN_DIR/controller/state）"
"$CONTROLLER_BIN" \
  --state-dir "$RUN_DIR/controller/state" \
  --audit-log "$RUN_DIR/controller/audit.log" \
  --snapshot-root "$SNAPSHOT_ROOT" \
  --listen 127.0.0.1:8889 &
CONTROLLER_OURS=1
echo $! > "$RUN_DIR/controller/pid"
controller_up=0
for _ in $(seq 1 60); do
  if curl -fsS "$FORKD_URL/v1/snapshots" >/dev/null 2>&1; then
    controller_up=1
    break
  fi
  sleep 1
done
if [ "$controller_up" -ne 1 ]; then
  log "controller 60s 未就绪，audit 日志："
  tail -n 40 "$RUN_DIR/controller/audit.log" 2>/dev/null || true
  exit 1
fi
log "controller 就绪（pid $(cat "$RUN_DIR/controller/pid")）"

# ---- snapshot-create（薄封装委托官方 forkd；rootfs 会被复制为快照私有副本，
# artifact 原件保持 pristine），随后轮询 list 直到 status=ready 且 bootable=true。
log "snapshot-create tag=$TAG"
FORKD_URL="$FORKD_URL" FORKD_BIN="$FORKD_BIN_DEFAULT" \
FORKD_KERNEL="$KERNEL_BIN" FORKD_ROOTFS="$ROOTFS_IMG" FORKD_TAP="$TAP_DEV" \
  "$CLI_BIN" forkd snapshot-create --tag "$TAG" --tap "$TAP_DEV" --json \
  | tee "$RUN_DIR/snapshot-create.json"

log "等待快照 ready+bootable（最长 300s）"
ready=0
for _ in $(seq 1 60); do
  if curl -fsS "$FORKD_URL/v1/snapshots" | python3 -c '
import json, sys
tag = sys.argv[1]
snaps = json.load(sys.stdin)
ok = any(s.get("tag") == tag
         and str(s.get("status", "")).lower() == "ready"
         and s.get("bootable") is True for s in snaps)
sys.exit(0 if ok else 1)
' "$TAG"; then
    ready=1
    break
  fi
  sleep 5
done
[ "$ready" -eq 1 ] || { log "快照 $TAG 未进入 ready+bootable，audit："; tail -n 40 "$RUN_DIR/controller/audit.log" || true; exit 1; }
log "快照 $TAG 就绪"

# ---- 跑真机 E2E：三重门（unix+cli cfg / #[ignore] / RFB_REAL_E2E=1）。
# --test-threads=1 是真机 E2E 的明确例外：真实 VM/快照栈无法安全并行，
# 与 CI（e2e.yml）保持一致；仓库“测试默认并行”规则针对单元测试。
log "cargo test --test forkd_real_e2e（串行）"
cd "$WORKSPACE"
export RFB_REAL_E2E=1
export RFB_E2E_SNAPSHOT_TAG="$TAG"
export RFB_E2E_ROOTFS_DIR="$ROOTFS_DIR"
export FORKD_URL="$FORKD_URL"
export FORKD_SNAPSHOT_TAG="$TAG"
export FORKD_BIN="$FORKD_BIN_DEFAULT"
export FORKD_KERNEL="$KERNEL_BIN"
export FORKD_ROOTFS="$ROOTFS_IMG"
export FORKD_TAP="$TAP_DEV"
cargo test -p rfb --features cli --test forkd_real_e2e -- --ignored --test-threads=1

log "PASS：全部真机 E2E 测试通过（REAL_KVM_E2E 证据）"
