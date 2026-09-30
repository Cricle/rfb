"""Quickstart for the published `rfb-sdk` package (PyPI) — both transports,
one flow (the UNIFIED_API contract: identical shapes on either backend).

Prerequisites:
  * forkd (default): a running controller (FORKD_URL / FORKD_TOKEN, default
    http://127.0.0.1:8889) with a ready snapshot, e.g.
    `rfb-cli forkd snapshot-create --tag rfb --tap forkd-tap0`;
  * zeroboot: a running ZBRT bridge (RFB_ZBRT_TCP, default
    127.0.0.1:15000) — start one with rfbsample's `app.py --up` or
    `rfb-cli zeroboot up`.

Install: `pip install rfb-sdk`
Run:     `python quickstart.py [--backend zeroboot] [rfb]`
"""

import argparse
import os
import sys

from rfb_sdk import RfbClient, RfbError, Sandbox, SandboxInfo


def flow(sandbox: Sandbox) -> None:
    """The SAME calls on either backend — shapes never change."""
    print("ping:", sandbox.ping())
    result = sandbox.exec(["echo", "hello"], cwd="/workspace")
    print("exec: exit=%d stdout=%s" % (result.exit_code, result.stdout_text.strip()))
    written = sandbox.write("notes.txt", b"hello from rfb-sdk")
    print("written:", written, "bytes")
    file = sandbox.read("notes.txt")
    print("read back", len(file.data), "bytes")
    print("ls:", [entry.name for entry in sandbox.ls("/workspace")])


def forkd_flow(tag: str) -> None:
    # FORKD_URL / FORKD_TOKEN / 10s timeout are the defaults.
    client = RfbClient()
    # Block until the snapshot reports status=ready and bootable=true.
    snapshot = client.wait_snapshot(tag)
    print(f"snapshot {snapshot.tag} is ready")
    sandbox = client.create_sandbox(tag)[0]
    print(f"sandbox {sandbox.id} created")
    try:
        # Guarded: a mid-flow failure must not leak a live sandbox.
        flow(sandbox)
    finally:
        sandbox.delete()
        print("sandbox deleted")


def zeroboot_flow() -> None:
    # Direct attach: the bridge speaks ZBRT at the guest agent; there is no
    # controller, so there is nothing to create or delete — the bridge's
    # runner owns the VM lifecycle.
    tcp = os.environ.get("RFB_ZBRT_TCP", "127.0.0.1:15000")
    sandbox = Sandbox(
        SandboxInfo(id="zeroboot-direct", guest_addr=tcp),
        RfbClient(), transport="zbrt")
    print(f"direct ZBRT sandbox at {tcp}")
    flow(sandbox)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--backend", choices=["forkd", "zeroboot"],
                        default="forkd")
    parser.add_argument("tag", nargs="?", default="rfb")
    args = parser.parse_args()
    if args.backend == "zeroboot":
        zeroboot_flow()
    else:
        forkd_flow(args.tag)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RfbError as error:
        print(f"rfb error: {error}", file=sys.stderr)
        raise SystemExit(1)
