/**
 * Quickstart for the published `rfb-sdk` package (npm) — both transports,
 * one flow (the UNIFIED_API contract: identical shapes on either backend).
 * The flow body is shared data: this file interprets
 * sdk/shared/conformance/example-flow.json, the same file every other
 * language's quickstart reads.
 *
 * Prerequisites:
 *   * forkd (default): a running controller (FORKD_URL / FORKD_TOKEN, default
 *     http://127.0.0.1:8889) with a ready snapshot, e.g. created with
 *     `rfb-cli forkd snapshot-create --tag rfb --tap forkd-tap0`;
 *   * zeroboot: a running ZBRT bridge (RFB_ZBRT_TCP, default
 *     127.0.0.1:15000) — start one with python/repl.py `--up` or
 *     `rfb-cli zeroboot up`.
 *
 * Install: `npm install rfb-sdk`
 * Run:     `node quickstart.mjs [--backend zeroboot] [rfb]`
 */
import { readFileSync } from 'node:fs';
import { RfbClient, RfbError, Sandbox, TRANSPORT_ZBRT } from 'rfb-sdk';

const [backend, tag] = (() => {
  const args = process.argv.slice(2);
  const i = args.indexOf('--backend');
  const backend = i >= 0 ? args[i + 1] : 'forkd';
  if (i >= 0) args.splice(i, 2);
  return [backend, args[0] ?? 'rfb'];
})();

/** The SAME calls on either backend — the scenario is shared data. */
async function flow(sandbox) {
  const spec = JSON.parse(readFileSync(
    new URL('../../shared/conformance/example-flow.json', import.meta.url),
    'utf8'));
  for (const op of spec.ops) {
    if (op.op === 'ping') {
      console.log('ping:', await sandbox.ping());
    } else if (op.op === 'exec') {
      const r = await sandbox.exec(op.argv, { cwd: op.cwd ?? '/workspace' });
      console.log(`exec: exit=${r.exitCode} stdout=${r.stdoutText.trim()}`);
    } else if (op.op === 'write') {
      console.log('written:', await sandbox.write(op.path, op.text), 'bytes');
    } else if (op.op === 'read') {
      const f = await sandbox.read(op.path);
      console.log('read back', f.data.length, 'bytes');
    } else if (op.op === 'ls') {
      const entries = await sandbox.ls(op.path);
      console.log('ls:', entries.map((entry) => entry.name));
    } else {
      throw new Error(`unknown op ${op.op}`);
    }
  }
}

try {
  if (backend === 'zeroboot') {
    // Direct attach: the bridge speaks ZBRT at the guest agent; no controller,
    // so nothing to create or delete — the bridge's runner owns the VM.
    const tcp = process.env.RFB_ZBRT_TCP ?? '127.0.0.1:15000';
    const sandbox = new Sandbox(
      { id: 'zeroboot-direct', guest_addr: tcp },
      new RfbClient(),
      TRANSPORT_ZBRT,
    );
    console.log(`direct ZBRT sandbox at ${tcp}`);
    await flow(sandbox);
  } else {
    // FORKD_URL / FORKD_TOKEN / 10s timeout are the defaults.
    const client = new RfbClient();
    const snapshot = await client.waitSnapshot(tag);
    console.log(`snapshot ${snapshot.tag} is ready`);
    const [sandbox] = await client.createSandbox(tag);
    console.log(`sandbox ${sandbox.id} created`);
    try {
      // Guarded: a mid-flow failure must not leak a live sandbox.
      await flow(sandbox);
    } finally {
      await sandbox.delete();
      console.log('sandbox deleted');
    }
  }
} catch (error) {
  if (error instanceof RfbError) {
    console.error(`rfb error: ${error.message}`);
    process.exit(1);
  }
  throw error;
}
// The controller's keep-alive socket holds the event loop open after the
// flow — exit explicitly on success.
process.exit(0);
