/**
 * Quickstart for the published `rfb-sdk` package (npm).
 *
 * Prerequisites:
 *   * a running forkd controller (default http://127.0.0.1:8889, or set
 *     FORKD_URL / FORKD_TOKEN);
 *   * a ready + bootable snapshot, e.g. created with
 *     `rfb-cli forkd snapshot-create --tag rfb --tap forkd-tap0`.
 *
 * Install: `npm install rfb-sdk`
 * Run:     `node quickstart.mjs rfb`
 */
import { RfbClient, RfbError } from 'rfb-sdk';

const tag = process.argv[2] ?? 'rfb';

try {
  // FORKD_URL / FORKD_TOKEN / 10s timeout are the defaults.
  const client = new RfbClient();

  // Block until the snapshot reports status=ready and bootable=true.
  const snapshot = await client.waitSnapshot(tag);
  console.log(`snapshot ${snapshot.tag} is ready`);

  const [sandbox] = await client.createSandbox(tag);
  console.log(`sandbox ${sandbox.id} created`);

  console.log('ping:', await sandbox.ping());

  const result = await sandbox.exec(['echo', 'hello'], { cwd: '/workspace' });
  console.log(`exec: exit=${result.exitCode} stdout=${result.stdoutText.trim()}`);

  const written = await sandbox.write('notes.txt', 'hello from rfb-sdk');
  console.log('written:', written, 'bytes');

  const file = await sandbox.read('notes.txt');
  console.log('read back', file.data.length, 'bytes');

  const entries = await sandbox.ls('/workspace');
  console.log('ls:', entries.map((entry) => entry.name));

  await sandbox.delete();
  console.log('sandbox deleted');
} catch (error) {
  if (error instanceof RfbError) {
    console.error(`rfb error: ${error.message}`);
    process.exit(1);
  }
  throw error;
}
