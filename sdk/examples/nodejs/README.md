# rfb-sdk Node.js quickstart

Uses the **published** `rfb-sdk` package from npm (no path/workspace
references). Run matrix, backends and the shared flow spec:
see [../README.md](../README.md).

```bash
npm install rfb-sdk
node quickstart.mjs [--backend forkd|zeroboot] rfb
```

The flow body is interpreted from `sdk/shared/conformance/example-flow.json`
— the same data file every other language's quickstart reads.
