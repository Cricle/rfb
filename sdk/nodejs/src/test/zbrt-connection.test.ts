import { describe, it, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import { ZbrtFrame, KIND_EXECUTE, KIND_OUTPUT, KIND_EXIT, KIND_HEALTH, KIND_HEALTH_ACK, KIND_CANCEL, KIND_CANCEL_ACK, KIND_FS, KIND_FS_RESULT, decode } from '../zbrt-frame.js';
import * as codec from '../zbrt-codec.js';
import { ZbrtConnection } from '../zbrt-connection.js';
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
        void handler(socket);
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

  it('execute collects output and returns exit', async () => {
    await startServer(async (socket) => {
      const frame = decode(await readFrame(socket));
      assert.equal(frame.kind, KIND_EXECUTE);
      const exec = codec.decodeExecute(frame.payload);
      assert.deepEqual(exec.argv, ['echo', 'hi']);
      const rid = frame.requestId;
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
  });

  it('health roundtrips', async () => {
    await startServer(async (socket) => {
      const frame = decode(await readFrame(socket));
      assert.equal(frame.kind, KIND_HEALTH);
      socket.write(frameBytes(KIND_HEALTH_ACK, frame.requestId, codec.encodeHealth(true, 'ready')));
    });
    const conn = new ZbrtConnection('127.0.0.1', port, 5000);
    await conn.ready();
    const health = await conn.health();
    assert.equal(health.healthy, true);
    assert.equal(health.message, 'ready');
    conn.close();
  });

  it('cancel roundtrips with empty CancelAck', async () => {
    await startServer(async (socket) => {
      const frame = decode(await readFrame(socket));
      assert.equal(frame.kind, KIND_CANCEL);
      socket.write(frameBytes(KIND_CANCEL_ACK, frame.requestId, Buffer.alloc(0)));
    });
    const conn = new ZbrtConnection('127.0.0.1', port, 5000);
    await conn.ready();
    await conn.cancel('test', Buffer.alloc(16, 1));
    conn.close();
  });

  it('fs roundtrips payload', async () => {
    await startServer(async (socket) => {
      const frame = decode(await readFrame(socket));
      const fs = codec.decodeFs(frame.payload);
      assert.equal(fs.op, 4);
      assert.equal(fs.path, 'test.txt');
      socket.write(frameBytes(KIND_FS_RESULT, frame.requestId, Buffer.from(JSON.stringify({ data: [104, 105], truncated: false, total_bytes: 2 }))));
    });
    const conn = new ZbrtConnection('127.0.0.1', port, 5000);
    await conn.ready();
    const result = await conn.fs(4, 'test.txt', Buffer.from(JSON.stringify({ offset: null, max_bytes: 100 })));
    const parsed = JSON.parse(result.toString('utf8'));
    assert.deepEqual(parsed.data, [104, 105]);
    conn.close();
  });

  it('raises RemoteError on Error frame', async () => {
    await startServer(async (socket) => {
      const frame = decode(await readFrame(socket));
      socket.write(frameBytes(12, frame.requestId, codec.encodeError(1, 'guest error')));
    });
    const conn = new ZbrtConnection('127.0.0.1', port, 5000);
    await conn.ready();
    await assert.rejects(
      () => conn.execute(['echo'], '/workspace', Buffer.alloc(0), 5000),
      (error: Error) => error.message.includes('guest error'),
    );
    conn.close();
  });
});

/** Read exactly one frame (header + payload) from the socket. */
async function readFrame(socket: net.Socket): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    let buf = Buffer.alloc(0);
    const onData = (chunk: Buffer) => {
      buf = Buffer.concat([buf, chunk]);
      if (buf.length < 28) return;
      const len = buf.readUInt32BE(24);
      if (buf.length < 28 + len) return;
      socket.off('data', onData);
      socket.off('error', onError);
      resolve(buf.subarray(0, 28 + len));
    };
    const onError = (e: Error) => {
      socket.off('data', onData);
      socket.off('error', onError);
      reject(e);
    };
    socket.on('data', onData);
    socket.on('error', onError);
  });
}
