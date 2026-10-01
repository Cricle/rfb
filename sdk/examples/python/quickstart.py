"""Quickstart: the same five sandbox calls on forkd (default, controller
snapshot) or zeroboot (direct ZBRT attach). Backends and run matrix:
see ../README.md.

Install: `pip install rfb-sdk`
Run:     `python quickstart.py [--backend zeroboot] [rfb]`
"""

import argparse
import os
import sys

from rfb_sdk import RfbClient, RfbError, Sandbox, SandboxInfo


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--backend", choices=["forkd", "zeroboot"],
                        default="forkd")
    parser.add_argument("tag", nargs="?", default="rfb")
    args = parser.parse_args()

    client = RfbClient()  # FORKD_URL / FORKD_TOKEN / 10s timeout defaults
    if args.backend == "zeroboot":
        # Direct attach: the bridge speaks ZBRT at the guest agent; the
        # bridge's runner owns the VM — nothing to create or delete.
        tcp = os.environ.get("RFB_ZBRT_TCP", "127.0.0.1:15000")
        sandbox = Sandbox(
            SandboxInfo(id="zeroboot-direct", guest_addr=tcp),
            client, transport="zbrt")
    else:
        client.wait_snapshot(args.tag)
        sandbox = client.create_sandbox(args.tag)[0]
    print(f"sandbox {sandbox.id} via {args.backend}")

    try:
        print("ping:", sandbox.ping())
        result = sandbox.exec(["echo", "hello"], cwd="/workspace")
        print("exec: exit=%d stdout=%s"
              % (result.exit_code, result.stdout_text.strip()))
        print("written:", sandbox.write("notes.txt", b"hello from rfb-sdk"),
              "bytes")
        print("read back", len(sandbox.read("notes.txt").data), "bytes")
        print("ls:", [entry.name for entry in sandbox.ls("/workspace")])
    finally:
        if args.backend != "zeroboot":
            # Guarded: a mid-flow failure must not leak a live sandbox.
            sandbox.delete()
            print("sandbox deleted")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RfbError as error:
        print(f"rfb error: {error}", file=sys.stderr)
        raise SystemExit(1)
