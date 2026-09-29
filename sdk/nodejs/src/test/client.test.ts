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
  it('ndjson exec rejects non-empty stdin before connecting', async () => {
    // The NDJSON wire has no exec stdin channel: non-empty stdin would run
    // the command WITHOUT its input, so it fails closed (stdin is ZBRT-only).
    const info = { id: 'sb-1', guest_addr: '127.0.0.1:1' };
    const sandbox = new Sandbox(info, {} as never, 'ndjson');
    await assert.rejects(
      () => sandbox.exec(['echo'], { stdin: Buffer.from('x') }),
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

  it('percent-encodes snapshot tags verbatim (dots and colons stay legal)', async () => {
    const { base, requests } = await startController((_url, res) => {
      res.setHeader('Content-Type', 'application/json');
      res.end('{"tag":"base.v2","status":"ready","bootable":true}');
    });
    const client = new RfbClient({ baseUrl: base, timeoutS: 5 });

    const dotted = await client.snapshot('base.v2');
    assert.equal(dotted?.status, 'ready');
    assert.equal(requests[0], 'GET /v1/snapshots/base.v2/info');

    await client.snapshot('snap:1');
    assert.equal(requests[1], 'GET /v1/snapshots/snap%3A1/info');
  });

  it('rejects an empty snapshot tag before any HTTP request', async () => {
    const { base, requests } = await startController((_url, res) => res.end('[]'));
    const client = new RfbClient({ baseUrl: base, timeoutS: 5 });
    const isValidation = (e: Error) => e.constructor.name === 'ValidationError';
    await assert.rejects(() => client.snapshot(''), isValidation);
    await assert.rejects(() => client.waitSnapshot(''), isValidation);
    await assert.rejects(() => client.createSandbox(''), isValidation);
    assert.equal(requests.length, 0, 'tag validation must precede any HTTP call');
  });

  it('waitSnapshot matches tags verbatim and createSandbox passes them in the body', async () => {
    const { base, requests } = await startController((url, res) => {
      res.setHeader('Content-Type', 'application/json');
      if (url === '/v1/snapshots') {
        res.end('[{"tag":"snap:1","status":"ready","bootable":true}]');
        return;
      }
      res.end('[{"id":"sb-2","snapshot_tag":"snap:1","guest_addr":"127.0.0.1:1"}]');
    });
    const client = new RfbClient({ baseUrl: base, timeoutS: 5 });

    const ready = await client.waitSnapshot('snap:1');
    assert.equal(ready.tag, 'snap:1');

    const sandboxes = await client.createSandbox('snap:1');
    assert.equal(sandboxes[0]?.id, 'sb-2');
    assert.deepEqual(requests, ['GET /v1/snapshots', 'POST /v1/sandboxes']);
  });

  it('passes controller ping payloads through verbatim (protocol_version)', async () => {
    const { base, requests } = await startController((_url, res) => {
      res.setHeader('Content-Type', 'application/json');
      res.end('{"pong":true,"protocol_version":1}');
    });
    const client = new RfbClient({ baseUrl: base, timeoutS: 5 });
    const value = await client.pingSandbox('sb-1');
    assert.deepEqual(value, { pong: true, protocol_version: 1 });
    assert.deepEqual(requests, ['POST /v1/sandboxes/sb-1/ping']);
  });
});
