/**
 * Sandbox facade over the NDJSON transport against an in-process fake guest:
 * exec/eval read budgets, whole-second wire timeouts, read optional keys and
 * the FORKD_AGENT_TOKEN auth line.
 */
import { describe, it, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import { Sandbox, TRANSPORT_NDJSON } from '../client.js';

describe('Sandbox over NDJSON (fake guest)', () => {
  let server: net.Server | null = null;
  const sockets: net.Socket[] = [];

  afterEach(() => {
    for (const socket of sockets) socket.destroy();
    sockets.length = 0;
    server?.close();
    server = null;
    delete process.env.FORKD_AGENT_TOKEN;
  });

  async function startGuest(
    onLine: (line: string, socket: net.Socket, index: number) => void,
  ): Promise<number> {
    server = net.createServer((socket) => {
      sockets.push(socket);
      socket.on('error', () => {});
      socket.setNoDelay(true);
      let buffer = '';
      let index = 0;
      socket.on('data', (chunk: Buffer) => {
        buffer += chunk.toString('utf8');
        let nl = buffer.indexOf('\n');
        while (nl >= 0) {
          const line = buffer.slice(0, nl);
          buffer = buffer.slice(nl + 1);
          onLine(line, socket, index);
          index += 1;
          nl = buffer.indexOf('\n');
        }
      });
    });
    await new Promise<void>((resolve) => server?.listen(0, '127.0.0.1', resolve));
    (server as unknown as net.Server).unref();
    return (server!.address() as net.AddressInfo).port;
  }

  function sandboxOn(port: number, guestTimeoutMs = 5000): Sandbox {
    return new Sandbox(
      { id: 'sb-1', guest_addr: `127.0.0.1:${port}` },
      {} as never,
      TRANSPORT_NDJSON,
      guestTimeoutMs,
    );
  }

  it('exec timeout is whole seconds on the wire and the read budget covers the guest', { timeout: 10_000 }, async () => {
    const lines: string[] = [];
    const port = await startGuest((line, socket, index) => {
      lines.push(line);
      if (index === 0) {
        // Reply well after the client timeout (200 ms) but inside the read
        // budget (client + exec + 5 s): the guest's own deadline owns exec.
        setTimeout(() => socket.write('{"exit_code":0,"stdout":"hi","timed_out":false}\n'), 400);
      }
    });
    const result = await sandboxOn(port, 200).exec(['echo'], { timeoutS: 1.2 });
    assert.equal(result.exitCode, 0);
    assert.equal(result.stdout.toString('utf8'), 'hi');
    assert.deepEqual(JSON.parse(lines[0] as string), {
      action: 'exec',
      cwd: '/workspace',
      timeout: 2,
      args: ['echo'],
    });
  });

  it('keeps the plain client timeout for non-exec requests', { timeout: 10_000 }, async () => {
    const port = await startGuest((_line, socket, index) => {
      if (index === 0) {
        setTimeout(() => socket.write('{"pong":true}\n'), 400);
      }
    });
    await assert.rejects(
      () => sandboxOn(port, 200).ping(),
      (error: Error) => error.constructor.name === 'TransportError',
    );
  });

  it('eval timeout is whole seconds and gets the same read margin', { timeout: 10_000 }, async () => {
    const lines: string[] = [];
    const port = await startGuest((line, socket, index) => {
      lines.push(line);
      if (index === 0) {
        setTimeout(
          () => socket.write('{"output":"2","status":0,"timed_out":false}\n'),
          400,
        );
      }
    });
    const result = await sandboxOn(port, 200).eval('1+1', { timeoutS: 1.5 });
    assert.equal(result.exitCode, 0);
    assert.equal(result.stdout.toString('utf8'), '2');
    assert.deepEqual(JSON.parse(lines[0] as string), {
      action: 'eval',
      code: '1+1',
      timeout: 2,
    });
  });

  it('read sends max_bytes/offset only when requested', { timeout: 10_000 }, async () => {
    const lines: string[] = [];
    const port = await startGuest((line, socket, index) => {
      lines.push(line);
      // NDJSON 连接池下两次 read 复用同一条连接（index 0/1 都要应答——
      // 真 agent 的 serve 循环就是多请求循环）。
      if (index <= 1) socket.write('{"data":"hi","truncated":false,"total_bytes":2}\n');
    });
    const sandbox = sandboxOn(port);
    const plain = await sandbox.read('notes.txt');
    assert.equal(plain.data.toString('utf8'), 'hi');
    assert.equal(plain.totalBytes, 2);
    assert.deepEqual(JSON.parse(lines[0] as string), { action: 'read', path: 'notes.txt' });

    await sandbox.read('notes.txt', { offset: 2, maxBytes: 10 });
    assert.deepEqual(JSON.parse(lines[1] as string), {
      action: 'read',
      path: 'notes.txt',
      offset: 2,
      max_bytes: 10,
    });

    // An out-of-range explicit cap still fails closed (no connection).
    const before = lines.length;
    await assert.rejects(
      () => sandbox.read('notes.txt', { maxBytes: 51201 }),
      (error: Error) => error.constructor.name === 'ValidationError',
    );
    assert.equal(lines.length, before);
  });

  it('authenticates every NDJSON connection when FORKD_AGENT_TOKEN is set', { timeout: 10_000 }, async () => {
    process.env.FORKD_AGENT_TOKEN = 'secret-token';
    const lines: string[] = [];
    const port = await startGuest((line, socket, index) => {
      lines.push(line);
      if (index === 0) {
        assert.deepEqual(JSON.parse(line), { action: 'auth', token: 'secret-token' });
        socket.write('{"action":"auth","ok":true}\n');
      } else if (index === 1) {
        socket.write('{"pong":true}\n');
      }
    });
    assert.equal(await sandboxOn(port).ping(), true);
    assert.equal(lines.length, 2);
    assert.deepEqual(JSON.parse(lines[1] as string), { action: 'ping' });
  });

  it('a rejected agent auth raises RemoteError before the real request', { timeout: 10_000 }, async () => {
    process.env.FORKD_AGENT_TOKEN = 'bad-token';
    const lines: string[] = [];
    const port = await startGuest((line, socket, index) => {
      lines.push(line);
      if (index === 0) socket.write('{"action":"auth","ok":false}\n');
    });
    await assert.rejects(
      () => sandboxOn(port).ping(),
      (error: Error) =>
        error.constructor.name === 'RemoteError' && error.message.includes('auth failed'),
    );
    assert.equal(lines.length, 1, 'the real action must never go on the wire');
  });

  it('leaves the wire unchanged when no agent token is configured', { timeout: 10_000 }, async () => {
    const lines: string[] = [];
    const port = await startGuest((line, socket, index) => {
      lines.push(line);
      if (index === 0) socket.write('{"pong":true}\n');
    });
    assert.equal(await sandboxOn(port).ping(), true);
    assert.deepEqual(JSON.parse(lines[0] as string), { action: 'ping' });
  });
});
