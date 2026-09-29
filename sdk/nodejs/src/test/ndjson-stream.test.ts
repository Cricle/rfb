import { describe, it, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import { Sandbox } from '../client.js';

/**
 * NDJSON interactive stream against an in-process fake guest (PROTOCOL.md §2.5).
 * Regression coverage for the socket lifecycle: the stream must use exactly one
 * TCP connection for its whole lifetime.
 */
describe('NdjsonGuestStream (fake NDJSON guest)', () => {
  let server: net.Server | null = null;
  let sockets: net.Socket[] = [];
  let connectionCount = 0;

  afterEach(() => {
    for (const socket of sockets) socket.destroy();
    sockets = [];
    server?.close();
    server = null;
    connectionCount = 0;
  });

  function sandboxOn(port: number): Sandbox {
    return new Sandbox({ id: 'sb-1', guest_addr: `127.0.0.1:${port}` }, {} as never, 'ndjson', 5000);
  }

  async function startGuest(
    onLine: (line: string, socket: net.Socket, index: number) => void,
  ): Promise<number> {
    server = net.createServer((socket) => {
      connectionCount += 1;
      sockets.push(socket);
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

  it('streams started/stdout/stderr/exit over a single connection', async () => {
    const requests: string[] = [];
    const port = await startGuest((line, socket, index) => {
      requests.push(line);
      if (index === 0) {
        socket.write('{"stream":"started"}\n');
        socket.write('{"stdout":"hi"}\n');
        socket.write('{"stderr":"err"}\n');
        socket.write('{"exit_code":3}\n');
      }
    });
    const stream = await sandboxOn(port).stream(['cat']);
    assert.equal((await stream.nextEvent())?.kind, 'started');
    const stdout = await stream.nextEvent();
    assert.equal(stdout?.kind, 'stdout');
    assert.equal(stdout?.data.toString('utf8'), 'hi');
    const stderr = await stream.nextEvent();
    assert.equal(stderr?.kind, 'stderr');
    assert.equal(stderr?.data.toString('utf8'), 'err');
    const exit = await stream.nextEvent();
    assert.equal(exit?.kind, 'exit');
    assert.equal(exit?.code, 3);
    assert.equal(await stream.nextEvent(), null);
    assert.equal(connectionCount, 1, 'the stream must reuse one TCP connection');
    assert.equal(requests.length, 1);
    assert.deepEqual(JSON.parse(requests[0] as string), { action: 'stream', args: ['cat'] });
  });

  it('accepts legacy out/err keys and the event/started form', async () => {
    const port = await startGuest((_line, socket, index) => {
      if (index === 0) {
        socket.write('{"event":"started"}\n');
        socket.write('{"out":"o"}\n');
        socket.write('{"err":"e"}\n');
        socket.write('{"done":true}\n');
      }
    });
    const stream = await sandboxOn(port).stream(['cat']);
    assert.equal((await stream.nextEvent())?.kind, 'started');
    assert.equal((await stream.nextEvent())?.data.toString('utf8'), 'o');
    assert.equal((await stream.nextEvent())?.data.toString('utf8'), 'e');
    const exit = await stream.nextEvent();
    assert.equal(exit?.kind, 'exit');
    assert.equal(exit?.code, null);
  });

  it('delivers sendInput and an idempotent stop on the same connection', async () => {
    const requests: string[] = [];
    const port = await startGuest((line, socket, index) => {
      requests.push(line);
      if (index === 0) socket.write('{"started":true}\n');
      if (line.includes('"in"')) socket.write('{"stdout":"echo:x"}\n');
      if (line.includes('"stop"')) socket.write('{"exit_code":0}\n');
    });
    const stream = await sandboxOn(port).stream(['cat']);
    await stream.nextEvent(); // started
    await stream.sendInput('x');
    const echoed = await stream.nextEvent();
    assert.equal(echoed?.data.toString('utf8'), 'echo:x');
    await stream.stop();
    const exit = await stream.nextEvent();
    assert.equal(exit?.kind, 'exit');
    await stream.stop(); // idempotent: no second stop frame
    assert.equal(connectionCount, 1);
    assert.deepEqual(requests.slice(1).map((line) => JSON.parse(line)), [
      { in: 'x' },
      { action: 'stop' },
    ]);
  });

  it('rejects sendInput after the stream ended', async () => {
    const port = await startGuest((_line, socket) => {
      socket.write('{"exit_code":0}\n');
    });
    const stream = await sandboxOn(port).stream(['cat']);
    assert.equal((await stream.nextEvent())?.kind, 'exit');
    await assert.rejects(
      () => stream.sendInput('late'),
      (error: Error) => error.constructor.name === 'RemoteError',
    );
  });

  it('surfaces a guest error line as RemoteError', async () => {
    const port = await startGuest((_line, socket) => {
      socket.write('{"error":"boom"}\n');
    });
    const stream = await sandboxOn(port).stream(['cat']);
    await assert.rejects(
      () => stream.nextEvent(),
      (error: Error) => error.constructor.name === 'RemoteError',
    );
  });

  it('rejects a stream that closes mid-line', async () => {
    const port = await startGuest((_line, socket) => {
      socket.write('{"stdout":"partial"'); // no trailing newline
      socket.end();
    });
    const stream = await sandboxOn(port).stream(['cat']);
    await assert.rejects(
      () => stream.nextEvent(),
      (error: Error) => error.constructor.name === 'DecodeError',
    );
  });

  it('rejects an oversized unterminated line', async () => {
    const port = await startGuest((_line, socket) => {
      socket.write(`{"stdout":"${'a'.repeat(1024 * 1024 + 16)}`);
    });
    const stream = await sandboxOn(port).stream(['cat']);
    await assert.rejects(
      () => stream.nextEvent(),
      (error: Error) => error.constructor.name === 'DecodeError',
    );
  });

  it('forwards cwd/pty/env only when requested', async () => {
    const requests: string[] = [];
    const port = await startGuest((line, socket) => {
      requests.push(line);
      socket.write('{"exit_code":0}\n');
    });
    const first = await sandboxOn(port).stream(['x']);
    await first.nextEvent();
    const second = await sandboxOn(port).stream(['x'], {
      cwd: '/workspace/sub',
      pty: true,
      env: { K: 'V' },
    });
    await second.nextEvent();
    assert.deepEqual(JSON.parse(requests[0] as string), { action: 'stream', args: ['x'] });
    assert.deepEqual(JSON.parse(requests[1] as string), {
      action: 'stream',
      args: ['x'],
      cwd: '/workspace/sub',
      pty: true,
      env: { K: 'V' },
    });
  });
});
