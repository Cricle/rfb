#!/bin/bash
# 一次性搭好 repl 示例的 assets/（本机构建，产物不入 git）。
# 前置：本仓库的 release 构建产物（target/x86_64-unknown-linux-musl/...，
# 由 `rfb-cli image build-static` 产出）+ resx/。
set -e
source $HOME/.cargo/env 2>/dev/null || export PATH=$HOME/.cargo/bin:$PATH
cd "$(dirname "$0")/../../.."   # → 仓库根
R=root/rfbsample-assets
A=sdk/examples/python/assets
mkdir -p "$R/pid1" "$A"
# firecracker：resx 的 tgz 内为 release-v1.16.1-x86_64/ 子目录
[ -f "$A/firecracker" ] || {
  tar -xzf resx/firecracker/firecracker-v1.16.1-x86_64.tgz -C /tmp     release-v1.16.1-x86_64/firecracker-v1.16.1-x86_64
  cp /tmp/release-v1.16.1-x86_64/firecracker-v1.16.1-x86_64 "$A/firecracker"
}
[ -f "$A/vmlinux" ] || cp resx/kernel/vmlinux-arcbox-0.0.24 "$A/vmlinux"
[ -f sdk/examples/python/assets/forkd.gz ] || gzip -9 -c resx/forkd/forkd > "$A/forkd.gz"
[ -f sdk/examples/python/assets/forkd-controller.gz ] || gzip -9 -c resx/forkd/forkd-controller > "$A/forkd-controller.gz"
# pid1 散件（rootfs 烤入用）
[ -f "$R/pid1/rfb-runtime" ] || ./target/release/rfb-cli image build-static --root . \
  --target x86_64-unknown-linux-musl --package rfb-runtime --features cli,rustpython
cp target/x86_64-unknown-linux-musl/release/rfb-runtime "$R/pid1/rfb-runtime"
cp target/x86_64-unknown-linux-musl/release/rfb-busybox "$R/pid1/rfb-busybox"
cp target/x86_64-unknown-linux-musl/release/rfb-mini-tools "$R/pid1/rfb-mini-tools"
chmod 755 "$R/pid1/"*
# 两个 rootfs（幂等：已存在即跳过）
[ -f "$A/zeroboot-zbrt.ext4.gz" ] || {
  ./target/release/rfb-cli image build-rootfs "$R/pid1/rfb-runtime" "$R/zb.ext4" \
    --mode zeroboot-zbrt --with-python --force
  gzip -9 -c "$R/zb.ext4" > "$A/zeroboot-zbrt.ext4.gz"
}
[ -f "$A/forkd-agent.ext4.gz" ] || {
  ./target/release/rfb-cli image build-rootfs "$R/pid1/rfb-runtime" "$R/fd.ext4" \
    --mode forkd-agent --with-python --force
  gzip -9 -c "$R/fd.ext4" > "$A/forkd-agent.ext4.gz"
}
ls -la "$A"
echo ASSETS_READY
