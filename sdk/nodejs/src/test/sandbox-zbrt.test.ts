/**
 * Sandbox facade over the ZBRT transport against an in-process fake guest:
 * mandatory Hello, synthesized started event, stdin/pty fail-closed behavior,
 * read args (null optionals), stop drain and exec timeout rounding.
 */
import { describe, it, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import {
  ZbrtFrame,
  KIND_CANCEL,
  KIND_CANCEL_ACK,
  KIND_EXECUTE,
  KIND_EXIT,
  KIND_FS,
  KIND_FS_RESULT,
  KIND_HELLO,
  KIND_HELLO_ACK,
  KIND_OUTPUT,
  frameReader,
} from '../zbrt-frame.js';
import * as codec from '../zbrt-codec.js';
import { V1_CAPABILITIES } from '../zbrt-connection.js';
import { Sandbox, TRANSPORT_ZBRT } from '../client.js';

interface ZbrtGuestRecord {
  hellos: codec.HelloLike[];
  executes: codec.Execute[];
  fs: { op: number; path: string; args: Record<string, unknown> }[];
  cancels: number;
}

function frameBytes(kind: number, reqId: Buffer, payload: Buffer): Buffer {
  return new ZbrtFrame(kind, 0, reqId, payload).encode();
}

describe('Sandbox over ZBRT (fake guest)', () => {
  const sockets: net.Socket[] = [];
  let server: net.Server | null = null;

  afterEach(() => {
    for (const socket of sockets) socket.destroy();
    sockets.length = 0;
    server?.close();
    server = null;
  });

  /**
   * Fake ZBRT guest: answers the mandatory Hello on every connection, records
   * inbound frames and delegates the rest to the per-test handler.
   */
  async function startGuest(
    handler: (frame: ZbrtFrame, socket: net.Socket, rec: ZbrtGuestRecord) => void,
  ): Promise<{ port: number; rec: ZbrtGuestRecord }> {
    const rec: ZbrtGuestRecord = { hellos: [], executes: [], fs: [], cancels: 0 };
    server = net.createServer((socket) => {
      sockets.push(socket);
      void (async () => {
        const reader = frameReader(socket);
        try {
          while (true) {
            const frame = await reader.next();
            if (frame === null) return;
            if (frame.kind === KIND_HELLO) {
              rec.hellos.push(codec.decodeHello(frame.payload));
              socket.write(
                frameBytes(
                  KIND_HELLO_ACK,
                  frame.requestId,
                  codec.encodeHelloAck('rfb-zeroboot-guest', V1_CAPABILITIES),
                ),
              );
              continue;
            }
            if (frame.kind === KIND_EXECUTE) rec.executes.push(codec.decodeExecute(frame.payload));
            if (frame.kind === KIND_FS) {
              const fs = codec.decodeFs(frame.payload);
              rec.fs.push({
                op: fs.op,
                path: fs.path,
                args: JSON.parse(fs.data.toString('utf8')) as Record<string, unknown>,
              });
            }
            if (frame.kind === KIND_CANCEL) rec.cancels += 1;
            handler(frame, socket, rec);
          }
        } catch {
          // Client teardown (destroyed sockets) is normal in these tests.
        }
      })();
    });
    await new Promise<void>((resolve) => server?.listen(0, '127.0.0.1', resolve));
    (server as unknown as net.Server).unref();
    return { port: (server!.address() as net.AddressInfo).port, rec };
  }

  function sandboxOn(port: number, guestTimeoutMs = 5000): Sandbox {
    return new Sandbox(
      { id: 'sb-1', guest_addr: `127.0.0.1:${port}` },
      {} as never,
      TRANSPORT_ZBRT,
      guestTimeoutMs,
    );
  }

  it('stream emits a synthesized started before Output/Exit', { timeout: 10_000 }, async () => {
    const { port, rec } = await startGuest((frame, socket) => {
      if (frame.kind !== KIND_EXECUTE) return;
      socket.write(frameBytes(KIND_OUTPUT, frame.requestId, codec.encodeOutput(0, Buffer.from('hi'))));
      socket.write(frameBytes(KIND_OUTPUT, frame.requestId, codec.encodeOutput(1, Buffer.from('err'))));
      socket.write(frameBytes(KIND_EXIT, frame.requestId, codec.encodeExit(0, null)));
    });
    const stream = await sandboxOn(port).stream(['echo']);
    const started = await stream.nextEvent();
    assert.equal(started?.kind, 'started');
    assert.equal(started?.data.length, 0);
    assert.equal(started?.code, null);
    const stdout = await stream.nextEvent();
    assert.equal(stdout?.kind, 'stdout');
    assert.equal(stdout?.data.toString('utf8'), 'hi');
    const stderr = await stream.nextEvent();
    assert.equal(stderr?.kind, 'stderr');
    assert.equal(stderr?.data.toString('utf8'), 'err');
    const exit = await stream.nextEvent();
    assert.equal(exit?.kind, 'exit');
    assert.equal(exit?.code, 0);
    assert.equal(await stream.nextEvent(), null);
    // The facade connection performed the mandatory Hello handshake.
    assert.equal(rec.hellos.length, 1);
    assert.equal(rec.hellos[0]?.client, 'rfb-sdk-node');
  });

  it('sendInput over ZBRT raises RemoteError (never Transport)', { timeout: 10_000 }, async () => {
    const { port } = await startGuest((frame, socket) => {
      if (frame.kind === KIND_EXECUTE) {
        socket.write(frameBytes(KIND_OUTPUT, frame.requestId, codec.encodeOutput(0, Buffer.from('x'))));
        socket.write(frameBytes(KIND_EXIT, frame.requestId, codec.encodeExit(0, null)));
      }
    });
    const stream = await sandboxOn(port).stream(['tail']);
    await assert.rejects(
      () => stream.sendInput('x'),
      (error: Error) =>
        error.constructor.name === 'RemoteError' &&
        error.message.includes('stdin is not supported'),
    );
    await stream.nextEvent(); // started
    await stream.nextEvent(); // stdout
    await stream.nextEvent(); // exit
    await assert.rejects(
      () => stream.sendInput('late'),
      (error: Error) =>
        error.constructor.name === 'RemoteError' && error.message.includes('no longer running'),
    );
  });

  it('stop drains straggler output and caches a first-arriving Exit', { timeout: 10_000 }, async () => {
    let held: Buffer | null = null;
    const { port, rec } = await startGuest((frame, socket) => {
      if (frame.kind === KIND_EXECUTE) {
        held = frame.requestId; // hold the turn open
        return;
      }
      if (frame.kind === KIND_CANCEL && held !== null) {
        socket.write(frameBytes(KIND_OUTPUT, held, codec.encodeOutput(0, Buffer.from('late'))));
        socket.write(frameBytes(KIND_EXIT, held, codec.encodeExit(-1, null)));
        socket.write(frameBytes(KIND_CANCEL_ACK, frame.requestId, Buffer.alloc(0)));
        held = null;
      }
    });
    const stream = await sandboxOn(port).stream(['tail']);
    assert.equal((await stream.nextEvent())?.kind, 'started');
    await stream.stop();
    const late = await stream.nextEvent();
    assert.equal(late?.kind, 'stdout');
    assert.equal(late?.data.toString('utf8'), 'late');
    const exit = await stream.nextEvent();
    assert.equal(exit?.kind, 'exit');
    assert.equal(exit?.code, -1);
    assert.equal(rec.cancels, 1);
    await stream.stop(); // idempotent: no second Cancel frame
    assert.equal(rec.cancels, 1);
  });

  it('read without maxBytes sends null optionals over ZBRT', { timeout: 10_000 }, async () => {
    const { port, rec } = await startGuest((frame, socket) => {
      if (frame.kind === KIND_FS) {
        socket.write(
          frameBytes(
            KIND_FS_RESULT,
            frame.requestId,
            Buffer.from(JSON.stringify({ data: [104, 105], truncated: false, total_bytes: 2 })),
          ),
        );
      }
    });
    const sandbox = sandboxOn(port);
    const result = await sandbox.read('notes.txt');
    assert.equal(result.data.toString('utf8'), 'hi');
    assert.deepEqual(rec.fs[0]?.args, { offset: null, max_bytes: null });

    await sandbox.read('notes.txt', { offset: 2, maxBytes: 10 });
    assert.deepEqual(rec.fs[1]?.args, { offset: 2, max_bytes: 10 });

    // Out-of-range explicit caps still fail closed before any frame.
    const before = rec.hellos.length;
    await assert.rejects(
      () => sandbox.read('notes.txt', { maxBytes: 0 }),
      (error: Error) => error.constructor.name === 'ValidationError',
    );
    assert.equal(rec.hellos.length, before);
  });

  it('rounds exec timeouts to whole seconds ×1000 and clamps to u32::MAX', { timeout: 10_000 }, async () => {
    const { port, rec } = await startGuest((frame, socket) => {
      if (frame.kind === KIND_EXECUTE) {
        socket.write(frameBytes(KIND_EXIT, frame.requestId, codec.encodeExit(0, null)));
      }
    });
    const sandbox = sandboxOn(port);
    await sandbox.exec(['echo'], { timeoutS: 0.25 });
    await sandbox.exec(['echo'], { timeoutS: 1.2 });
    await sandbox.exec(['echo'], { timeoutS: 10_000_000 });
    assert.equal(rec.executes[0]?.timeoutMs, 1000, 'ceil(0.25) * 1000');
    assert.equal(rec.executes[1]?.timeoutMs, 2000, 'ceil(1.2) * 1000');
    assert.equal(rec.executes[2]?.timeoutMs, 0xffff_ffff, 'clamped to u32::MAX');
  });

  it('delivers exec stdin inside the Execute payload', { timeout: 10_000 }, async () => {
    const { port, rec } = await startGuest((frame, socket) => {
      if (frame.kind === KIND_EXECUTE) {
        socket.write(frameBytes(KIND_EXIT, frame.requestId, codec.encodeExit(0, null)));
      }
    });
    await sandboxOn(port).exec(['cat'], { stdin: Buffer.from('abc') });
    assert.deepEqual(rec.executes[0]?.stdin, Buffer.from('abc'));
  });

  it('ls over ZBRT sends op 1 and decodes entries', { timeout: 10_000 }, async () => {
    const { port, rec } = await startGuest((frame, socket) => {
      if (frame.kind === KIND_FS) {
        socket.write(
          frameBytes(
            KIND_FS_RESULT,
            frame.requestId,
            Buffer.from(
              JSON.stringify({
                entries: [
                  { name: 'a.txt', is_dir: false, size: 2 },
                  { name: 'sub', is_dir: true, size: null },
                ],
              }),
            ),
          ),
        );
      }
    });
    const entries = await sandboxOn(port).ls('docs');
    assert.deepEqual(entries, [
      { name: 'a.txt', isDir: false, size: 2 },
      { name: 'sub', isDir: true, size: null },
    ]);
    assert.equal(rec.fs[0]?.op, 1);
    assert.equal(rec.fs[0]?.path, 'docs');
    assert.equal(rec.fs[0]?.args.max_results, 1000);
  });

  it('find over ZBRT sends op 2 and decodes string matches', { timeout: 10_000 }, async () => {
    const { port, rec } = await startGuest((frame, socket) => {
      if (frame.kind === KIND_FS) {
        socket.write(
          frameBytes(
            KIND_FS_RESULT,
            frame.requestId,
            Buffer.from(JSON.stringify({ matches: ['a.txt', 'sub/b.txt'] })),
          ),
        );
      }
    });
    const matches = await sandboxOn(port).find('.', '*.txt');
    assert.deepEqual(matches, ['a.txt', 'sub/b.txt']);
    assert.equal(rec.fs[0]?.op, 2);
    assert.equal(rec.fs[0]?.path, '.');
    assert.equal(rec.fs[0]?.args.pattern, '*.txt');
    assert.equal(rec.fs[0]?.args.max_results, 1000);
  });

  it('grep over ZBRT sends op 3 and decodes match objects', { timeout: 10_000 }, async () => {
    const { port, rec } = await startGuest((frame, socket) => {
      if (frame.kind === KIND_FS) {
        socket.write(
          frameBytes(
            KIND_FS_RESULT,
            frame.requestId,
            Buffer.from(
              JSON.stringify({
                matches: [{ path: 'a.txt', line: 3, column: 1, text: 'needle' }],
              }),
            ),
          ),
        );
      }
    });
    const matches = await sandboxOn(port).grep('.', 'needle');
    assert.deepEqual(matches, [{ path: 'a.txt', line: 3, column: 1, text: 'needle' }]);
    assert.equal(rec.fs[0]?.op, 3);
    assert.equal(rec.fs[0]?.path, '.');
    assert.equal(rec.fs[0]?.args.pattern, 'needle');
    assert.equal(rec.fs[0]?.args.max_results, 1000);
    assert.equal(rec.fs[0]?.args.max_bytes, 50 * 1024);
  });
});
