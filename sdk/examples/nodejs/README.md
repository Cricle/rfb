# rfb-sdk Node.js quickstart

Uses the **published** `rfb-sdk` package from npm (no path/workspace references).

```bash
npm install rfb-sdk
node quickstart.mjs rfb     # tag of a ready+bootable snapshot
```

Prerequisites: a running forkd controller (`FORKD_URL`, default
`http://127.0.0.1:8889`) and a snapshot created with
`rfb-cli forkd snapshot-create --tag rfb --tap forkd-tap0`.

The example walks the unified scenario: wait for the snapshot, create one
sandbox, ping, exec, write/read a file, list the workspace, delete.
