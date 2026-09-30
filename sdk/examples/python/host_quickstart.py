#!/usr/bin/env python3
"""Host-side quickstart: boot the BACKEND from Python — no controller, no
TAP, no rfb-cli, no cargo. Requires: KVM + root, the firecracker/vmlinux/
zeroboot-zbrt.ext4(.gz) assets in this directory, and `pip install rfb-sdk`.

Run:  sudo python3 host_quickstart.py
"""

import os

from rfb_sdk import ZerobootHost

# Point RFB_DEMO_ASSETS at a directory holding firecracker[.gz] /
# vmlinux[.gz] / zeroboot-zbrt.ext4[.gz] (release assets, or made with
# `rfb-cli image build-rootfs ... --mode zeroboot-zbrt`).
ASSETS = os.environ.get("RFB_DEMO_ASSETS",
                        os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                     "assets"))


def main() -> int:
    host = ZerobootHost(
        tcp=os.environ.get("RFB_ZBRT_TCP", "127.0.0.1:15000"),
        assets_dir=ASSETS,
        run_dir="/root/rfb-host-demo",
    )
    host.up()  # idempotent: a healthy VM is reused
    sandbox = host.sandbox()  # the unified facade — identical shapes to forkd

    print("ping:", sandbox.ping())
    result = sandbox.exec(["echo", "hello from a direct ZBRT sandbox"],
                          cwd="/workspace")
    print("exec: exit=%d stdout=%s" % (result.exit_code, result.stdout_text.strip()))

    written = sandbox.write("notes.txt", b"hello from rfb_sdk.host")
    file = sandbox.read("notes.txt")
    print("written %d bytes, read back %d bytes" % (written, len(file.data)))
    print("ls:", [entry.name for entry in sandbox.ls("/workspace")])

    host.down()  # kill the VM + the TCP bridge
    print("vm down")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
