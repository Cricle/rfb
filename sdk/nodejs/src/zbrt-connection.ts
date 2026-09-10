/**
 * One ZBRT v1 session over a TCP socket (PROTOCOL.md §3.4, mirroring
 * `zeroboot_connection.rs` / the Java ZbrtConnection): fresh 128-bit request
 * id per request, Output frames strictly before exactly one terminal frame,
 * idempotent Cancel with empty-payload CancelAck, Error frames raise
 * RemoteError. INTERNAL.
 */
import net from 'node:net';
import { randomBytes } from 'node:crypto';
import { DecodeError, RemoteError, TransportError } from './errors.js';
import {
  KIND_CANCEL,
  KIND_CANCEL_ACK,
  KIND_ERROR,
  KIND_EXECUTE,
  KIND_EXIT,
  KIND_FS,
  KIND_FS_RESULT,
  KIND_HELLO,
  KIND_HELLO_ACK,
  KIND_HEALTH,
  KIND_HEALTH_ACK,
  KIND_OUTPUT,
  ZbrtFrame,
  frameReader,
} from './zbrt-frame.js';
import * as codec from './zbrt-codec.js';

export const V1_CAPABILITIES: readonly string[] = Object.freeze([
  'execute',
  'stream',
  'deadline',
  'health',
  'cancel',
  'filesystem',
]);

function connectSocket(host: string, port: number, timeoutMs: number): Promise<net.Socket> {
  return new Promise<net.Socket>((resolve, reject) => {
    const socket = net.createConnection({ host, port });
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new TransportError('guest connection timed out'));
    }, timeoutMs);
    socket.once('connect', () => {
      clearTimeout(timer);
      socket.setNoDelay(true);
      socket.setTimeout(0);
      resolve(socket);
    });
    socket.once('error', (error: Error) => {
      clearTimeout(timer);
      reject(new TransportError(`guest connect failed: ${error.message}`));
    });
    socket.once('timeout', () => {
      clearTimeout(timer);
      socket.destroy();
      reject(new TransportError('guest connection timed out'));
    });
  });
}

/** Internal frame-source seam so the session can reach connection privates. */
interface ZbrtSessionHost {
  nextFrame(): Promise<ZbrtFrame | null>;
  requireId(frame: ZbrtFrame, requestId: Buffer): Promise<void>;
  errorFromFrame(frame: ZbrtFrame): RemoteError;
  unexpectedFrame(frame: ZbrtFrame, expected: string): DecodeError;
  cancelRoundTrip(reason: string | null, target16: Buffer | null): Promise<ZbrtFrame>;
}

/** One active Execute turn: yields Output events, then the terminal Exit. */
export class ZbrtStreamSession {
  public readonly requestId: Buffer;
  public stopped = false;
  public terminal = false;
  readonly #host: ZbrtSessionHost;

  constructor(host: ZbrtSessionHost, requestId: Buffer) {
    this.#host = host;
    this.requestId = requestId;
  }

  /** Next event: {stream, data, code} — code != null marks the Exit. */
  async nextEvent(): Promise<{ stream: number; data: Buffer; code: number | null } | null> {
    if (this.terminal) return null;
    const frame = await this.#host.nextFrame();
    if (frame === null) return null;
    await this.#host.requireId(frame, this.requestId);
    if (frame.kind === KIND_OUTPUT) {
      const output = codec.decodeOutput(frame.payload);
      return { stream: output.stream, data: output.data, code: null };
    }
    if (frame.kind === KIND_EXIT) {
      const exit = codec.decodeExit(frame.payload);
      this.terminal = true;
      return { stream: -1, data: Buffer.alloc(0), code: exit.code };
    }
    if (frame.kind === KIND_ERROR) {
      throw this.#host.errorFromFrame(frame);
    }
    throw this.#host.unexpectedFrame(frame, 'Output/Exit');
  }

  /** Idempotent stop: Cancel targeting this request, expecting CancelAck. */
  async stop(): Promise<void> {
    if (this.terminal || this.stopped) return;
    this.stopped = true;
    const ack = await this.#host.cancelRoundTrip(null, this.requestId);
    if (ack.payload.length !== 0) {
      throw new DecodeError('CancelAck payload must be empty');
    }
  }
}

export class ZbrtConnection {
  #socket: net.Socket | null = null;
  #reader: ReturnType<typeof frameReader> | undefined = undefined;
  #ready: Promise<void>;

  constructor(host: string, port: number, timeoutMs: number) {
    this.#ready = (async () => {
      this.#socket = await connectSocket(host, port, timeoutMs);
      this.#reader = frameReader(this.#socket);
    })();
  }

  /** Resolves once the TCP connection is up. */
  async ready(): Promise<this> {
    await this.#ready;
    return this;
  }

  newRequestId(): Buffer {
    return randomBytes(16);
  }

  async #writeFrame(frame: ZbrtFrame): Promise<void> {
    const socket = this.#socket;
    if (!socket) throw new TransportError('connection closed');
    await new Promise<void>((resolve, reject) => {
      socket.write(frame.encode(), (error) => (error ? reject(error) : resolve()));
    });
  }

  async requireId(frame: ZbrtFrame, requestId: Buffer): Promise<void> {
    if (!frame.requestId.equals(requestId)) {
      throw new DecodeError('request_id mismatch');
    }
  }

  errorFromFrame(frame: ZbrtFrame): RemoteError {
    const error = codec.decodeError(frame.payload);
    return new RemoteError(`${error.code}: ${error.message}`);
  }

  unexpectedFrame(frame: ZbrtFrame, expected: string): DecodeError {
    return new DecodeError(`unexpected frame kind ${frame.kind} while awaiting ${expected}`);
  }

  async #roundTrip(
    requestKind: number,
    payload: Buffer,
    ackKind: number,
    expected: string,
  ): Promise<ZbrtFrame> {
    const id = this.newRequestId();
    await this.#writeFrame(new ZbrtFrame(requestKind, 0, id, payload));
    const frame = await this.#reader?.next();
    if (frame === null || frame === undefined) throw new TransportError('connection closed by guest');
    await this.requireId(frame, id);
    if (frame.kind === ackKind) return frame;
    if (frame.kind === KIND_ERROR) throw this.errorFromFrame(frame);
    throw this.unexpectedFrame(frame, expected);
  }

  async #cancelRoundTrip(reason: string | null, target16: Buffer | null): Promise<ZbrtFrame> {
    const id = this.newRequestId();
    await this.#writeFrame(new ZbrtFrame(KIND_CANCEL, 0, id, codec.encodeCancel(reason, target16)));
    const frame = await this.#reader?.next();
    if (!frame) throw new TransportError('connection closed by guest');
    if (frame.kind === KIND_CANCEL_ACK) return frame;
    if (frame.kind === KIND_ERROR) throw this.errorFromFrame(frame);
    throw this.unexpectedFrame(frame, 'CancelAck');
  }

  /** Optional Hello handshake; the guest is auto-ready without it. */
  async hello(clientName: string): Promise<codec.HelloAck> {
    await this.#ready;
    const id = this.newRequestId();
    await this.#writeFrame(
      new ZbrtFrame(KIND_HELLO, 0, id, codec.encodeHello(clientName, V1_CAPABILITIES)),
    );
    const frame = await this.#reader?.next();
    if (frame === null || frame === undefined) throw new TransportError('connection closed by guest');
    if (frame.kind === KIND_HELLO_ACK) return codec.decodeHelloAck(frame.payload);
    throw this.unexpectedFrame(frame, 'HelloAck');
  }

  /**
   * Run one command: collect Output frames (0=stdout, 1=stderr) until exactly
   * one terminal frame — Exit (completed/cancelled) or Error (raise).
   * timeoutMs rides the wire as the guest-side deadline.
   */
  async execute(
    argv: readonly string[],
    cwd: string | null | undefined,
    stdin: Uint8Array,
    timeoutMs: number,
  ): Promise<{ code: number; stdout: Buffer; stderr: Buffer; signal: number | null; timedOut: boolean }> {
    await this.#ready;
    const id = this.newRequestId();
    await this.#writeFrame(
      new ZbrtFrame(
        KIND_EXECUTE,
        0,
        id,
        codec.encodeExecute(argv, cwd ?? null, stdin, timeoutMs),
      ),
    );
    const stdout: Buffer[] = [];
    const stderr: Buffer[] = [];
    while (true) {
      const frame = await this.#reader?.next();
      if (frame === null || frame === undefined) throw new TransportError('connection closed by guest');
      await this.requireId(frame, id);
      if (frame.kind === KIND_OUTPUT) {
        const output = codec.decodeOutput(frame.payload);
        (output.stream === 0 ? stdout : stderr).push(output.data);
      } else if (frame.kind === KIND_EXIT) {
        const exit = codec.decodeExit(frame.payload);
        return {
          code: exit.code,
          stdout: Buffer.concat(stdout),
          stderr: Buffer.concat(stderr),
          signal: exit.signal,
          timedOut: false,
        };
      } else if (frame.kind === KIND_ERROR) {
        throw this.errorFromFrame(frame);
      } else {
        throw this.unexpectedFrame(frame, 'Output/Exit');
      }
    }
  }

  /** Idempotent cancel; expects the empty-payload CancelAck. */
  async cancel(reason: string | null, target16: Buffer | null): Promise<void> {
    if (target16 !== null && target16.length !== 16) {
      throw new DecodeError('cancel target must be 16 bytes');
    }
    const frame = await this.#cancelRoundTrip(reason, target16);
    if (frame.payload.length !== 0) {
      throw new DecodeError('CancelAck payload must be empty');
    }
  }

  /** Health round-trip; the guest reports healthy=true, message="ready". */
  async health(): Promise<codec.Health> {
    await this.#ready;
    const frame = await this.#roundTrip(
      KIND_HEALTH,
      codec.encodeHealth(true, null),
      KIND_HEALTH_ACK,
      'HealthAck',
    );
    return codec.decodeHealth(frame.payload);
  }

  /** Route one filesystem RPC (opcodes 1=ls 2=find 3=grep 4=read 5=write). */
  async fs(op: number, path: string, jsonArgs: Buffer): Promise<Buffer> {
    await this.#ready;
    const frame = await this.#roundTrip(KIND_FS, codec.encodeFs(op, path, jsonArgs), KIND_FS_RESULT, 'FsResult');
    return frame.payload;
  }

  /**
   * Start an interactive turn: send Execute and return a session that yields
   * Output/Exit frames. Only one turn may be active per connection.
   */
  async openStreamSession(
    argv: readonly string[],
    cwd: string | null | undefined,
    stdin: Uint8Array,
    timeoutMs: number,
  ): Promise<ZbrtStreamSession> {
    await this.#ready;
    const id = this.newRequestId();
    await this.#writeFrame(
      new ZbrtFrame(KIND_EXECUTE, 0, id, codec.encodeExecute(argv, cwd ?? null, stdin, timeoutMs)),
    );
    const host: ZbrtSessionHost = {
      nextFrame: async () => (await Promise.resolve(this.#reader))?.next() ?? null,
      requireId: (frame, requestId) => this.requireId(frame, requestId),
      errorFromFrame: (frame) => this.errorFromFrame(frame),
      unexpectedFrame: (frame, expected) => this.unexpectedFrame(frame, expected),
      cancelRoundTrip: (reason, target) => this.#cancelRoundTrip(reason, target),
    };
    return new ZbrtStreamSession(host, id);
  }

  close(): void {
    this.#socket?.end();
    this.#socket?.destroy();
  }
}
