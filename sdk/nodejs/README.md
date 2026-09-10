# rfb-sdk (Node.js / TypeScript)

Node.js port of the RFB unified SDK — a mirror of the Rust reference
implementation `rfb::client::RfbClient`. One client (`RfbClient`), one
per-connection facade (`Sandbox`), dual guest transports (forkd TCP NDJSON and
the ZBRT v1 binary frame protocol) with identical method names and result
shapes. Zero runtime dependencies (Node.js >= 18 standard library only).

## Install

```bash
npm install rfb-sdk
```

## Quickstart

```js
import { RfbClient, RfbError } from 'rfb-sdk';

const client = new RfbClient(); // FORKD_URL / FORKD_TOKEN / 10s timeout defaults
const snapshot = await client.waitSnapshot('rfb');
const [sandbox] = await client.createSandbox('rfb');

console.log('ping:', await sandbox.ping());

const result = await sandbox.exec(['echo', 'hello'], { cwd: '/workspace' });
console.log(result.exitCode, result.stdoutText.trim());

await sandbox.write('notes.txt', 'hello');
const file = await sandbox.read('notes.txt');

await sandbox.delete();
```

A runnable version lives in [`../examples/nodejs/`](../examples/nodejs/).

## API surface

- `RfbClient` — forkd controller lifecycle over HTTP/JSON:
  `listSnapshots`, `snapshot`, `waitSnapshot`, `createSandbox(tag, {n, transport})`,
  `connect`, `connectWithTransport`, `pingSandbox`, `deleteSandbox`.
- `Sandbox` — guest operations over either transport:
  `ping`, `exec`, `eval`, `ls`, `find`, `grep`, `read`, `write`, `stream`, `delete`.
- `GuestStream` — interactive process streams: `nextEvent`, `sendInput` (NDJSON
  only — ZBRT v1 has no stdin channel), `stop`.
- Errors (catch `RfbError` for everything): `TransportError`, `HttpStatusError`,
  `DecodeError`, `RemoteError`, `ValidationError`.

Transports are selected per sandbox: `createSandbox(tag, { transport: 'ndjson' })`
(default) or `{ transport: 'zbrt' }`. Byte-level contracts are specified in
[`../PROTOCOL.md`](../PROTOCOL.md); the API table in
[`../UNIFIED_API.md`](../UNIFIED_API.md).

## Development

```bash
npm install
npm test     # build (tsc) + node --test over dist/test
```

The test suite covers validation fail-closed rules, ZBRT golden vectors and
strict-decode rejects, fake frame-server connection flows, and client
construction/policy edges.
