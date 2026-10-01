/**
 * Quickstart: the same five sandbox calls on forkd (default, controller
 * snapshot) or zeroboot (direct ZBRT attach). Backends and run matrix:
 * see ../README.md.
 *
 * Install: `npm install rfb-sdk`
 * Run:     `node quickstart.mjs [--backend zeroboot] [rfb]`
 */
import { RfbClient, RfbError, Sandbox, TRANSPORT_ZBRT } from 'rfb-sdk';

const [backend, tag] = (() => {
  const args = process.argv.slice(2);
  const i = args.indexOf('--backend');
  const b = i >= 0 ? args[i + 1] : 'forkd';
  if (i >= 0) args.splice(i, 2);
  return [b, args[0] ?? 'rfb'];
})();

try {
  const client = new RfbClient(); // FORKD_URL / FORKD_TOKEN / 10s timeout defaults
  const owned = backend !== 'zeroboot';
  let sandbox;
  if (owned) {
    sandbox = (await client.createSandbox((await client.waitSnapshot(tag)).tag))[0];
  } else {
    // Direct attach: the bridge speaks ZBRT at the guest agent; the bridge's
    // runner owns the VM — nothing to create or delete.
    const tcp = process.env.RFB_ZBRT_TCP ?? '127.0.0.1:15000';
    sandbox = new Sandbox({ id: 'zeroboot-direct', guest_addr: tcp }, client, TRANSPORT_ZBRT);
  }
  console.log(`sandbox ${sandbox.id} via ${backend}`);

  try {
    console.log('ping:', await sandbox.ping());
    const r = await sandbox.exec(['echo', 'hello'], { cwd: '/workspace' });
    console.log(`exec: exit=${r.exitCode} stdout=${r.stdoutText.trim()}`);
    console.log('written:', await sandbox.write('notes.txt', 'hello from rfb-sdk'), 'bytes');
    console.log('read back', (await sandbox.read('notes.txt')).data.length, 'bytes');
    console.log('ls:', (await sandbox.ls('/workspace')).map((e) => e.name));
  } finally {
    if (owned) {
      // Guarded: a mid-flow failure must not leak a live sandbox.
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
// undici's keep-alive socket holds the event loop — exit on success.
process.exit(0);
