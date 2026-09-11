"""Quickstart for the published `rfb-sdk` package (PyPI).

Prerequisites:
  * a running forkd controller (default http://127.0.0.1:8889, or set
    FORKD_URL / FORKD_TOKEN);
  * a ready + bootable snapshot, e.g. created with
    `rfb-cli forkd snapshot-create --tag rfb --tap forkd-tap0`.

Install: `pip install rfb-sdk`
Run:     `python quickstart.py rfb`
"""

import sys

from rfb_sdk import RfbClient, RfbError


def main() -> int:
    tag = sys.argv[1] if len(sys.argv) > 1 else "rfb"

    # FORKD_URL / FORKD_TOKEN / 10s timeout are the defaults.
    client = RfbClient()

    # Block until the snapshot reports status=ready and bootable=true.
    snapshot = client.wait_snapshot(tag)
    print(f"snapshot {snapshot.tag} is ready")

    sandbox = client.create_sandbox(tag)[0]
    print(f"sandbox {sandbox.id} created")

    print("ping:", sandbox.ping())

    result = sandbox.exec(["echo", "hello"], cwd="/workspace")
    print("exec: exit=%d stdout=%s" % (result.exit_code, result.stdout_text.strip()))

    written = sandbox.write("notes.txt", b"hello from rfb-sdk")
    print("written:", written, "bytes")

    file = sandbox.read("notes.txt")
    print("read back", len(file.data), "bytes")

    print("ls:", [entry.name for entry in sandbox.ls("/workspace")])

    sandbox.delete()
    print("sandbox deleted")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RfbError as error:
        print(f"rfb error: {error}", file=sys.stderr)
        raise SystemExit(1)
