"""The quickstart flow, shared by every Python example.

The scenario is DATA: sdk/shared/conformance/example-flow.json — every
language's example interprets the same file, so the five flows cannot
drift (UNIFIED_API.md's one contract, shown at demo time).
"""

import json
import pathlib

SPEC = json.loads(
    (pathlib.Path(__file__).resolve().parent.parent.parent
     / "shared" / "conformance" / "example-flow.json").read_text())


def run(sandbox) -> None:
    for op in SPEC["ops"]:
        kind = op["op"]
        if kind == "ping":
            print("ping:", sandbox.ping())
        elif kind == "exec":
            result = sandbox.exec(op["argv"], cwd=op.get("cwd", "/workspace"))
            print("exec: exit=%d stdout=%s"
                  % (result.exit_code, result.stdout_text.strip()))
        elif kind == "write":
            written = sandbox.write(op["path"], op["text"].encode())
            print("written:", written, "bytes")
        elif kind == "read":
            print("read back", len(sandbox.read(op["path"]).data), "bytes")
        elif kind == "ls":
            print("ls:", [entry.name for entry in sandbox.ls(op["path"])])
        else:
            raise ValueError(f"unknown op {kind!r}")
