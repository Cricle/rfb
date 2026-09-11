import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { RfbClient, Sandbox } from '../client.js';

describe('RfbClient construction', () => {
  it('resolves FORKD_URL env', () => {
    process.env.FORKD_URL = 'http://127.0.0.1:19999';
    const client = new RfbClient();
    assert.equal(client.baseUrl, 'http://127.0.0.1:19999');
    delete process.env.FORKD_URL;
  });
  it('defaults to http://127.0.0.1:8889', () => {
    const client = new RfbClient();
    assert.equal(client.baseUrl, 'http://127.0.0.1:8889');
  });
  it('rejects ftp scheme', () => {
    assert.throws(() => new RfbClient({ baseUrl: 'ftp://x' }), Error);
  });
  it('rejects non-positive timeout', () => {
    assert.throws(() => new RfbClient({ timeoutS: 0 }), Error);
    assert.throws(() => new RfbClient({ timeoutS: -1 }), Error);
  });
});

describe('Sandbox construction', () => {
  it('stores info and transport', () => {
    const info = { id: 'sb-1', snapshot_tag: 'snap-1', guest_addr: '127.0.0.1:19998' };
    const sandbox = new Sandbox(info, {} as never, 'ndjson');
    assert.equal(sandbox.id, 'sb-1');
    assert.equal(sandbox.snapshotTag, 'snap-1');
    assert.equal(sandbox.guestAddr, '127.0.0.1:19998');
    assert.equal(sandbox.transport, 'ndjson');
  });
  it('zbrt stream rejects pty before connecting', async () => {
    const info = { id: 'sb-1', guest_addr: '127.0.0.1:1' };
    const sandbox = new Sandbox(info, {} as never, 'zbrt');
    await assert.rejects(
      () => sandbox.stream(['echo'], { pty: true }),
      (e: Error) => e.constructor.name === 'ValidationError',
    );
  });
  it('zbrt stream rejects env before connecting', async () => {
    const info = { id: 'sb-1', guest_addr: '127.0.0.1:1' };
    const sandbox = new Sandbox(info, {} as never, 'zbrt');
    await assert.rejects(
      () => sandbox.stream(['echo'], { env: { K: 'V' } }),
      (e: Error) => e.constructor.name === 'ValidationError',
    );
  });
});
