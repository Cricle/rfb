/**
 * Quickstart for the published `rfb-sdk` package (npm) — both transports,
 * one flow (the UNIFIED_API contract: identical shapes on either backend).
 *
 * Prerequisites:
 *   * forkd (default): a running controller (FORKD_URL / FORKD_TOKEN, default
 *     http://127.0.0.1:8889) with a ready snapshot, e.g. created with
 *     `rfb-cli forkd snapshot-create --tag rfb --tap forkd-tap0`;
 *   * zeroboot: a running ZBRT bridge (RFB_ZBRT_TCP, default
 *     127.0.0.1:15000) — start one with rfbsample's `app.py --up` or
 *     `rfb-cli zeroboot up`.
 *
 * Install: `npm install rfb-sdk`
 * Run:     `node quickstart.mjs [--backend zeroboot] [rfb]`
 */
import { RfbClient, RfbError, Sandbox, TRANSPORT_ZBRT } from 'rfb-sdk';

const [backend, tag] = (() => {
  const args = process.argv.slice(2);
  const i = args.indexOf('--backend');
  const backend = i >= 0 ? args[i + 1] : 'forkd';
  if (i >= 0) args.splice(i, 2);
  return [backend, args[0] ?? 'rfb'];
})();

/** The SAME calls on either backend — shapes never change. */
async function flow(sandbox) {
  console.log('ping:', await sandbox.ping());
  const result = await sandbox.exec(['echo', 'hello'], { cwd: '/workspace' });
  console.log(`exec: exit=${result.exitCode} stdout=${result.stdoutText.trim()}`);
  const written = await sandbox.write('notes.txt', 'hello from rfb-sdk');
  console.log('written:', written, 'bytes');
  const file = await sandbox.read('notes.txt');
  console.log('read back', file.data.length, 'bytes');
  const entries = await sandbox.ls('/workspace');
  console.log('ls:', entries.map((entry) => entry.name));
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
