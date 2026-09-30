# rfb-sdk Python quickstart

Uses the **published** `rfb-sdk` package from PyPI (no path/workspace
references). Run matrix, backends and the shared flow spec:
see [../README.md](../README.md).

```bash
pip install rfb-sdk
python quickstart.py [--backend forkd|zeroboot] rfb
```

Python-only extras in this directory:

- `flow_common.py` — the shared scenario interpreter: it consumes
  `sdk/shared/conformance/example-flow.json`, the same data file every other
  language's quickstart reads, and is reused by both quickstarts below;
- `host_quickstart.py` — boots the backend itself via `rfb_sdk.host`
  (KVM + root, no controller/TAP/CLI): `sudo python3 host_quickstart.py`;
- `repl.py` — the interactive sandbox demo (dual backend,
  self-bootstrapping): `sudo python3 repl.py --up`; assets come from
  `bash setup-demo-assets.sh` (not in git);
- `test_repl.py` — unit tests for the demo and the shared interpreter
  (no VM needed).
