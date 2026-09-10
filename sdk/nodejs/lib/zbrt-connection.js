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

export const V1_CAPABILITIES = Object.freeze([
  'execute',
  'stream',
  'deadline',
  'health',
  'cancel',
  'filesystem',
]);

function connectSocket(host, port, timeoutMs) {
  return new Promise((resolve, reject) => {
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
    socket.once('error', (error) => {
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

/** Reader for one active Execute turn on a ZBRT connection. */
class ZbrtStreamSession {
  constructor(connection, requestId) {
    this.connection = connection;
    this.requestId = requestId;
    this.stopped = false;
    this.terminal = false;
  }

  /** Next event: {stream, data, code} — code != null marks the Exit. */
  async nextEvent() {
    if (this.terminal) return null;
    const frame = await this.connection.#reader.next();
    await this.connection.#requireId(frame, this.requestId);
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
      throw this.connection.#errorFrame(frame);
    }
    throw this.connection.#unexpected(frame, 'Output/Exit');
  }

  /** Idempotent stop: Cancel targeting this request, expecting CancelAck. */
  async stop() {
    if (this.terminal || this.stopped) return;
    this.stopped = true;
    const ack = await this.connection.#cancelRoundTrip(null, this.requestId);
    if (ack.payload.length !== 0) {
      throw new DecodeError('CancelAck payload must be empty');
    }
  }
}

export class ZbrtConnection {
  #socket = null;
  #reader = null;
  #ready;

  constructor(host, port, timeoutMs) {
    this.timeoutMs = timeoutMs;
    this.#ready = (async () => {
      this.#socket = await connectSocket(host, port, timeoutMs);
      this.#reader = frameReader(this.#socket);
    })();
  }

  /** Resolves once the TCP connection is up. */
  async ready() {
    await this.#ready;
    return this;
  }

  newRequestId() {
    return randomBytes(16);
  }

  async #writeFrame(frame) {
    if (!this.#socket) throw new TransportError('connection closed');
    await new Promise((resolve, reject) => {
      this.#socket.write(frame.encode(), (error) => (error ? reject(error) : resolve()));
    });
  }

  async #requireId(frame, requestId) {
    if (!frame.requestId.equals(requestId)) {
      throw new DecodeError('request_id mismatch');
    }
  }

  #errorFrame(frame) {
    const error = codec.decodeError(frame.payload);
    return new RemoteError(`${error.code}: ${error.message}`);
  }

  #unexpected(frame, expected) {
    return new DecodeError(`unexpected frame kind ${frame.kind} while awaiting ${expected}`);
  }

  async #cancelRoundTrip(reason, target16) {
    const id = this.newRequestId();
    await this.#writeFrame(new ZbrtFrame(KIND_CANCEL, 0, id, codec.encodeCancel(reason, target16)));
    const frame = await this.#reader.next();
    await this.#requireId(frame, id);
    if (frame.kind === KIND_CANCEL_ACK) return frame;
    if (frame.kind === KIND_ERROR) throw this.#errorFrame(frame);
    throw this.#unexpected(frame, 'CancelAck');
  }

  /** Optional Hello handshake; the guest is auto-ready without it. */
  async hello(clientName) {
    await this.#ready;
    const id = this.newRequestId();
    await this.#writeFrame(
      new ZbrtFrame(KIND_HELLO, 0, id, codec.encodeHello(clientName, V1_CAPABILITIES)),
    );
    const frame = await this.#reader.next();
    await this.#requireId(frame, id);
    if (frame.kind === KIND_HELLO_ACK) {
      return codec.decodeHelloAck(frame.payload);
    }
    throw this.#unexpected(frame, 'HelloAck');
  }

  /**
   * Run one command: collect Output frames (0=stdout, 1=stderr) until exactly
   * one terminal frame — Exit (completed/cancelled) or Error (raise).
   * timeoutMs rides the wire as the guest-side deadline.
   */
  async execute(argv, cwd, stdin, timeoutMs) {
    await this.#ready;
    const id = this.newRequestId();
    await this.#writeFrame(
      new ZbrtFrame(KIND_EXECUTE, 0, id, codec.encodeExecute(argv, cwd, stdin ?? Buffer.alloc(0), timeoutMs)),
    );
    const stdout = [];
    const stderr = [];
    while (true) {
      const frame = await this.#reader.next();
      await this.#requireId(frame, id);
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
        throw this.#errorFrame(frame);
      } else {
        throw this.#unexpected(frame, 'Output/Exit');
      }
    }
  }

  /** Idempotent cancel; expects the empty-payload CancelAck. */
  async cancel(reason, target16) {
    if (target16 !== null && target16 !== undefined && target16.length !== 16) {
      throw new DecodeError('cancel target must be 16 bytes');
    }
    const frame = await this.#cancelRoundTrip(reason, target16);
    if (frame.payload.length !== 0) {
      throw new DecodeError('CancelAck payload must be empty');
    }
  }

  /** Health round-trip; the guest reports healthy=true, message="ready". */
  async health() {
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
  async fs(op, path, jsonArgs) {
    await this.#ready;
    const frame = await this.#roundTrip(
      KIND_FS,
      codec.encodeFs(op, path, jsonArgs),
      KIND_FS_RESULT,
      'FsResult',
    );
    return frame.payload;
  }

  /**
   * Start an interactive turn: send Execute and return a session that yields
   * Output/Exit frames. Only one turn may be active per connection.
   */
  async openStreamSession(argv, cwd, stdin, timeoutMs) {
    await this.#ready;
    const id = this.newRequestId();
    await this.#writeFrame(
      new ZbrtFrame(KIND_EXECUTE, 0, id, codec.encodeExecute(argv, cwd, stdin ?? Buffer.alloc(0), timeoutMs)),
    );
    return new ZbrtStreamSession(this, id);
  }

  close() {
    this.#socket?.end();
    this.#socket?.destroy();
  }
}
