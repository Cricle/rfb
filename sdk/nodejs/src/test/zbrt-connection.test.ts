import { describe, it, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import {
  ZbrtFrame,
  KIND_EXECUTE,
  KIND_OUTPUT,
  KIND_EXIT,
  KIND_HEALTH,
  KIND_HEALTH_ACK,
  KIND_HELLO,
  KIND_HELLO_ACK,
  KIND_CANCEL,
  KIND_CANCEL_ACK,
  KIND_FS,
  KIND_FS_RESULT,
  frameReader,
  type FrameReader,
} from '../zbrt-frame.js';
import * as codec from '../zbrt-codec.js';
import {
  ZbrtConnection,
  MAX_TURN_OUTPUT_BYTES,
  V1_CAPABILITIES,
  ZBRT_CLIENT_NAME,
} from '../zbrt-connection.js';

function frameBytes(kind: number, reqId: Buffer, payload: Buffer): Buffer {
  return new ZbrtFrame(kind, 0, reqId, payload).encode();
}

describe('ZbrtConnection (fake frame server)', () => {
  let server: net.Server;
  let port = 0;
  let cleanup: (() => void) | null = null;

  afterEach(() => {
    cleanup?.();
    cleanup = null;
  });

  function startServer(handler: (socket: net.Socket) => Promise<void>): Promise<number> {
    return new Promise((resolve) => {
      server = net.createServer((socket) => {
        // Test-side teardown (destroyed sockets) must not crash the runner.
        void handler(socket).catch(() => {});
      });
      server.listen(0, '127.0.0.1', () => {
        const addr = server.address() as net.AddressInfo;
        port = addr.port;
        // unref: the fake server must not keep the test process alive once
        // the suite's own sockets are closed.
        server.unref();
        cleanup = () => {
          server.close();
        };
        resolve(port);
      });
    });
  }

  /**
   * The cross-language contract makes every ZBRT connection Hello-first: the
   * fake guest answers the handshake before any test-specific behavior.
   * Returns the frame reader (null when the handshake was answered negatively).
   */
  async function acceptHello(
    socket: net.Socket,
    received: ZbrtFrame[],
    reply: 'ack' | 'close' | 'wrong_kind' = 'ack',
  ): Promise<FrameReader | null> {
    const reader = frameReader(socket);
    const hello = await reader.next();
    if (hello === null) return null;
    received.push(hello);
    assert.equal(hello.kind, KIND_HELLO);
    if (reply === 'close') {
      socket.destroy();
      return null;
    }
    if (reply === 'wrong_kind') {
      socket.write(frameBytes(KIND_HEALTH_ACK, hello.requestId, codec.encodeHealth(true, null)));
      return null;
    }
    socket.write(
      frameBytes(
        KIND_HELLO_ACK,
        hello.requestId,
        codec.encodeHelloAck('rfb-zeroboot-guest', V1_CAPABILITIES),
      ),
    );
    return reader;
  }

  it('helloes every connection and executes after the handshake', { timeout: 10_000 }, async () => {
    const received: ZbrtFrame[] = [];
    await startServer(async (socket) => {
      const reader = await acceptHello(socket, received);
      const frame = await reader!.next();
      received.push(frame!);
      assert.equal(frame!.kind, KIND_EXECUTE);
      const exec = codec.decodeExecute(frame!.payload);
      assert.deepEqual(exec.argv, ['echo', 'hi']);
      const rid = frame!.requestId;
      socket.write(frameBytes(KIND_OUTPUT, rid, codec.encodeOutput(0, Buffer.from('hello'))));
      socket.write(frameBytes(KIND_OUTPUT, rid, codec.encodeOutput(1, Buffer.from('err'))));
      socket.write(frameBytes(KIND_EXIT, rid, codec.encodeExit(0, null)));
    });

    const conn = new ZbrtConnection('127.0.0.1', port, 5000);
    await conn.ready();
    const result = await conn.execute(['echo', 'hi'], '/workspace', Buffer.alloc(0), 5000);
    assert.equal(result.code, 0);
    assert.deepEqual(result.stdout, Buffer.from('hello'));
    assert.deepEqual(result.stderr, Buffer.from('err'));
    assert.equal(result.timedOut, false);
    conn.close();

    // First frame on the wire is Hello with the canonical client name + caps.
    const hello = codec.decodeHello(received[0]!.payload);
    assert.equal(received[0]!.kind, KIND_HELLO);
    assert.equal(hello.client, ZBRT_CLIENT_NAME);
    assert.deepEqual(hello.capabilities, V1_CAPABILITIES);
  });

  it('a rejected handshake is a TransportError and sends no request frame', { timeout: 10_000 }, async () => {
    const received: ZbrtFrame[] = [];
    await startServer(async (socket) => {
      await acceptHello(socket, received, 'wrong_kind');
    });

    const conn = new ZbrtConnection('127.0.0.1', port, 5000);
    await assert.rejects(
      () => conn.execute(['echo'], '/workspace', Buffer.alloc(0), 5000),
      (error: Error) => error.constructor.name === 'TransportError',
    );
    // Only the Hello ever went out: the session is unusable without HelloAck.
    assert.equal(received.length, 1);
    conn.close();
  });

  it('a closed handshake is a TransportError', { timeout: 10_000 }, async () => {
    const received: ZbrtFrame[] = [];
    await startServer(async (socket) => {
      await acceptHello(socket, received, 'close');
    });

    const conn = new ZbrtConnection('127.0.0.1', port, 5000);
    await assert.rejects(
      () => conn.ready(),
      (error: Error) => error.constructor.name === 'TransportError',
    );
    conn.close();
  });

  it('a read stall past the exec read budget is a TransportError', { timeout: 15_000 }, async () => {
    await startServer(async (socket) => {
      await acceptHello(socket, []);
      // Hold the turn open with no reply. During a turn the guest is
      // legitimately silent until its own deadline, so the session fails at
      // base + guest deadline + EXEC_READ_MARGIN (rust/java/python contract)
      // — bounded, never a hang (PROTOCOL.md §3.4).
    });

    const conn = new ZbrtConnection('127.0.0.1', port, 250);
    await conn.ready();
    await assert.rejects(
      () => conn.execute(['echo'], '/workspace', Buffer.alloc(0), 100),
      (error: Error) => error.constructor.name === 'TransportError',
    );
    conn.close();
  });

  it('health roundtrips', { timeout: 10_000 }, async () => {
    await startServer(async (socket) => {
      const reader = await acceptHello(socket, []);
      const frame = await reader!.next();
      assert.equal(frame!.kind, KIND_HEALTH);
      socket.write(frameBytes(KIND_HEALTH_ACK, frame!.requestId, codec.encodeHealth(true, 'ready')));
    });
    const conn = new ZbrtConnection('127.0.0.1', port, 5000);
    await conn.ready();
    const health = await conn.health();
    assert.equal(health.healthy, true);
    assert.equal(health.message, 'ready');
    conn.close();
  });

  it('cancel roundtrips with empty CancelAck', { timeout: 10_000 }, async () => {
    await startServer(async (socket) => {
      const reader = await acceptHello(socket, []);
      const frame = await reader!.next();
      assert.equal(frame!.kind, KIND_CANCEL);
      socket.write(frameBytes(KIND_CANCEL_ACK, frame!.requestId, Buffer.alloc(0)));
    });
    const conn = new ZbrtConnection('127.0.0.1', port, 5000);
    await conn.ready();
    await conn.cancel('test', Buffer.alloc(16, 1));
    conn.close();
  });

  it('fs roundtrips payload', { timeout: 10_000 }, async () => {
    await startServer(async (socket) => {
      const reader = await acceptHello(socket, []);
      const frame = await reader!.next();
      const fs = codec.decodeFs(frame!.payload);
      assert.equal(fs.op, 4);
      assert.equal(fs.path, 'test.txt');
      socket.write(frameBytes(KIND_FS_RESULT, frame!.requestId, Buffer.from(JSON.stringify({ data: [104, 105], truncated: false, total_bytes: 2 }))));
    });
    const conn = new ZbrtConnection('127.0.0.1', port, 5000);
    await conn.ready();
    const result = await conn.fs(4, 'test.txt', Buffer.from(JSON.stringify({ offset: null, max_bytes: 100 })));
    const parsed = JSON.parse(result.toString('utf8'));
    assert.deepEqual(parsed.data, [104, 105]);
    conn.close();
  });

  it('raises RemoteError on Error frame', { timeout: 10_000 }, async () => {
    await startServer(async (socket) => {
      const reader = await acceptHello(socket, []);
      const frame = await reader!.next();
      socket.write(frameBytes(12, frame!.requestId, codec.encodeError(1, 'guest error')));
    });
    const conn = new ZbrtConnection('127.0.0.1', port, 5000);
    await conn.ready();
    await assert.rejects(
      () => conn.execute(['echo'], '/workspace', Buffer.alloc(0), 5000),
      (error: Error) => error.message.includes('guest error'),
    );
    conn.close();
  });

  it('synthesizes a started event before the first stream frame', { timeout: 10_000 }, async () => {
    await startServer(async (socket) => {
      const reader = await acceptHello(socket, []);
      const frame = await reader!.next();
      const rid = frame!.requestId;
      socket.write(frameBytes(KIND_OUTPUT, rid, codec.encodeOutput(0, Buffer.from('hello\n'))));
      socket.write(frameBytes(KIND_EXIT, rid, codec.encodeExit(0, null)));
    });
    const conn = new ZbrtConnection('127.0.0.1', port, 5000);
    await conn.ready();
    const session = await conn.openStreamSession(['echo'], null, Buffer.alloc(0), 0);
    const started = await session.nextEvent();
    assert.equal(started?.started, true);
    assert.equal(started?.code, null);
    const output = await session.nextEvent();
    assert.equal(output?.stream, 0);
    assert.deepEqual(output?.data, Buffer.from('hello\n'));
    const exit = await session.nextEvent();
    assert.equal(exit?.code, 0);
    assert.equal(await session.nextEvent(), null);
    conn.close();
  });

  it('stop caches an Exit that arrives before the CancelAck', { timeout: 10_000 }, async () => {
    let held: Buffer | null = null;
    await startServer(async (socket) => {
      const reader = await acceptHello(socket, []);
      while (true) {
        const frame = await reader!.next();
        if (frame === null) return;
        if (frame.kind === KIND_EXECUTE) {
          held = frame.requestId; // hold the turn open
          continue;
        }
        if (frame.kind === KIND_CANCEL) {
          // Exit arrives BEFORE the CancelAck: stop() must not drop it.
          if (held !== null) {
            socket.write(frameBytes(KIND_EXIT, held, codec.encodeExit(-1, null)));
            held = null;
          }
          socket.write(frameBytes(KIND_CANCEL_ACK, frame.requestId, Buffer.alloc(0)));
          return;
        }
      }
    });
    const conn = new ZbrtConnection('127.0.0.1', port, 5000);
    await conn.ready();
    const session = await conn.openStreamSession(['tail'], null, Buffer.alloc(0), 0);
    await session.stop(); // must return on the CancelAck, not block
    const started = await session.nextEvent();
    assert.equal(started?.started, true);
    const exit = await session.nextEvent();
    assert.equal(exit?.code, -1);
    assert.equal(await session.nextEvent(), null);
    conn.close();
  });

  it('fails a turn whose aggregate output exceeds 16 MiB with a Remote error', { timeout: 30_000 }, async () => {
    const chunk = Buffer.alloc(1024 * 1024, 0x61);
    await startServer(async (socket) => {
      const reader = await acceptHello(socket, []);
      const frame = await reader!.next();
      const rid = frame!.requestId;
      // One byte over the cap across Output frames.
      for (let i = 0; i < MAX_TURN_OUTPUT_BYTES / chunk.length + 1; i++) {
        socket.write(frameBytes(KIND_OUTPUT, rid, codec.encodeOutput(0, chunk)));
      }
    });
    const conn = new ZbrtConnection('127.0.0.1', port, 10_000);
    await conn.ready();
    await assert.rejects(
      () => conn.execute(['yes'], '/workspace', Buffer.alloc(0), 10_000),
      (error: Error) =>
        error.constructor.name === 'RemoteError' && error.message.includes('output exceeded'),
    );
    conn.close();
  });
});
