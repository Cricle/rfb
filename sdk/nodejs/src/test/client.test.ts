import { describe, it, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
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

describe('RfbClient controller calls (fake HTTP controller)', () => {
  let server: http.Server | null = null;

  afterEach(() => {
    server?.close();
    server = null;
  });

  async function startController(
    handler: (url: string, res: http.ServerResponse) => void,
  ): Promise<{ base: string; requests: string[] }> {
    const requests: string[] = [];
    server = http.createServer((req, res) => {
      requests.push(`${req.method} ${req.url}`);
      handler(req.url ?? '', res);
    });
    await new Promise<void>((resolve) => server?.listen(0, '127.0.0.1', resolve));
    server.unref();
    const address = server.address() as { port: number };
    return { base: `http://127.0.0.1:${address.port}`, requests };
  }

  it('listSandboxes returns attachable Sandbox handles', async () => {
    const { base } = await startController((_url, res) => {
      res.setHeader('Content-Type', 'application/json');
      res.end('[{"id":"sb-1","snapshot_tag":"base","guest_addr":"127.0.0.1:1"}]');
    });
    const client = new RfbClient({ baseUrl: base, timeoutS: 5 });
    const sandboxes = await client.listSandboxes();
    assert.equal(sandboxes.length, 1);
    assert.equal(sandboxes[0]?.id, 'sb-1');
    assert.equal(sandboxes[0]?.guestAddr, '127.0.0.1:1');
  });

  it('connectWithTransport rejects an invalid transport before any request', async () => {
    const { base, requests } = await startController((_url, res) => res.end('[]'));
    const client = new RfbClient({ baseUrl: base, timeoutS: 5 });
    await assert.rejects(
      () => client.connectWithTransport('sb-1', 'grpc'),
      (e: Error) => e.constructor.name === 'ValidationError',
    );
    assert.equal(requests.length, 0, 'validation must precede any HTTP call');
  });

  it('a malformed 2xx body raises DecodeError (not SyntaxError)', async () => {
    const { base } = await startController((_url, res) => res.end('not json'));
    const client = new RfbClient({ baseUrl: base, timeoutS: 5 });
    await assert.rejects(
      () => client.listSnapshots(),
      (e: Error) => e.constructor.name === 'DecodeError',
    );
  });
});
