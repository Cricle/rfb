# rfb-sdk Python quickstart

Uses the **published** `rfb-sdk` package from PyPI (no path/workspace references).

```bash
pip install rfb-sdk
python quickstart.py rfb     # tag of a ready+bootable snapshot
```

Prerequisites: a running forkd controller (`FORKD_URL`, default
`http://127.0.0.1:8889`) and a snapshot created with
`rfb-cli forkd snapshot-create --tag rfb --tap forkd-tap0`.

The example walks the unified scenario: wait for the snapshot, create one
sandbox, ping, exec, write/read a file, list the workspace, delete.
