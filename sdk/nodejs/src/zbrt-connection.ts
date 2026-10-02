/**
 * One ZBRT v1 session over a TCP socket (PROTOCOL.md §3.4, mirroring
 * `zeroboot_connection.rs` / the Java ZbrtConnection): fresh 128-bit request
 * id per request, mandatory Hello handshake on connect, Output frames strictly
 * before exactly one terminal frame, idempotent Cancel with empty-payload
 * CancelAck, Error frames raise RemoteError. INTERNAL.
 */
import net from 'node:net';
import { randomBytes } from 'node:crypto';
import { DecodeError, RemoteError, TransportError, ValidationError } from './errors.js';
import { parseAddress } from './ndjson.js';
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

/** Client name announced in the mandatory Hello handshake. */
export const ZBRT_CLIENT_NAME = 'rfb-sdk-node';

/** Per-turn aggregate Output cap: one Execute turn may stream at most 16 MiB
 * of combined stdout+stderr before the SDK fails it with a Remote error. */
export const MAX_TURN_OUTPUT_BYTES = 16 * 1024 * 1024;

function connectSocket(host: string, port: number, timeoutMs: number): Promise<net.Socket> {
  return new Promise<net.Socket>((resolve, reject) => {
    const socket = net.createConnection({ host, port });
    let settled = false;
    const fail = (error: TransportError): void => {
      // destroy(error) surfaces through the frame reader as a TransportError;
      // reject() only matters while the connect handshake is still pending.
      socket.destroy(error);
      if (!settled) {
        settled = true;
        reject(error);
      }
    };
    const timer = setTimeout(() => {
      fail(new TransportError('guest connection timed out'));
    }, timeoutMs);
    socket.once('connect', () => {
      clearTimeout(timer);
      socket.setNoDelay(true);
      // Read-stall budget (PROTOCOL.md §3.4: 读停顿 → Transport): an idle gap
      // longer than the client timeout must fail the session, never hang it.
      socket.setTimeout(timeoutMs);
      settled = true;
      resolve(socket);
    });
    socket.once('error', (error: Error) => {
      clearTimeout(timer);
      if (!settled) {
        settled = true;
        socket.destroy();
        reject(new TransportError(`guest connect failed: ${error.message}`));
      }
    });
    socket.once('timeout', () => {
      clearTimeout(timer);
      fail(new TransportError('guest read timed out'));
    });
  });
}

/** Internal frame-source seam so the session can reach connection privates. */
interface ZbrtSessionHost {
  nextFrame(): Promise<ZbrtFrame | null>;
  requireId(frame: ZbrtFrame, requestId: Buffer): Promise<void>;
  errorFromFrame(frame: ZbrtFrame): RemoteError;
  unexpectedFrame(frame: ZbrtFrame, expected: string): DecodeError;
  cancelRoundTrip(
    reason: string | null,
    target16: Buffer | null,
    onStraggler?: (frame: ZbrtFrame) => void,
  ): Promise<ZbrtFrame>;
}

/** One streamed event: Output (stream 0/1) or the terminal Exit (code set).
 * The synthesized first event of a turn carries `started: true`. */
export interface ZbrtStreamEvent {
  stream: number;
  data: Buffer;
  code: number | null;
  started?: true;
}

/** One active Execute turn: yields Output events, then the terminal Exit. */
export class ZbrtStreamSession {
  public readonly requestId: Buffer;
  public stopped = false;
  public terminal = false;
  readonly #host: ZbrtSessionHost;
  #pending: ZbrtStreamEvent[] = [];
  #announced = false;
  #outputBytes = 0;

  constructor(host: ZbrtSessionHost, requestId: Buffer) {
    this.#host = host;
    this.requestId = requestId;
  }

  /** Next event: {stream, data, code} — code != null marks the Exit. */
  async nextEvent(): Promise<ZbrtStreamEvent | null> {
    if (!this.#announced) {
      // Cross-language contract: the first event of every ZBRT turn is a
      // synthesized started marker (the ZBRT wire has no started frame,
      // unlike the NDJSON stream in PROTOCOL.md §2.5).
      this.#announced = true;
      return { stream: -1, data: Buffer.alloc(0), code: null, started: true };
    }
    if (this.#pending.length > 0) return this.#pending.shift() as ZbrtStreamEvent;
    if (this.terminal) return null;
    const frame = await this.#host.nextFrame();
    if (frame === null) {
      // 流结束：置 terminal，否则消费 null 后的任何 nextEvent() 都会在
      // 已销毁 socket 的 frameReader 上永远 pending。
      this.terminal = true;
      return null;
    }
    await this.#host.requireId(frame, this.requestId);
    if (frame.kind === KIND_OUTPUT) {
      const output = codec.decodeOutput(frame.payload);
      return this.#takeOutput(output.stream, output.data);
    }
    if (frame.kind === KIND_EXIT) {
      const exit = codec.decodeExit(frame.payload);
      this.terminal = true;
      return { stream: -1, data: Buffer.alloc(0), code: exit.code };
    }
    if (frame.kind === KIND_ERROR) {
      // Error = 终态：置 terminal（ facade 随后 close 连接——参照
      // python _ZbrtStream 的 Error 处理）。
      this.terminal = true;
      throw this.#host.errorFromFrame(frame);
    }
    this.terminal = true;
    throw this.#host.unexpectedFrame(frame, 'Output/Exit');
  }

  /** Buffer one Output event and enforce the per-turn aggregate output cap. */
  #takeOutput(stream: number, data: Buffer): ZbrtStreamEvent {
    this.#outputBytes += data.length;
    if (this.#outputBytes > MAX_TURN_OUTPUT_BYTES) {
      throw new RemoteError(`zbrt turn output exceeded ${MAX_TURN_OUTPUT_BYTES} bytes`);
    }
    return { stream, data, code: null };
  }

  /** Idempotent stop: Cancel targeting this request, expecting CancelAck. */
  async stop(): Promise<void> {
    if (this.terminal || this.stopped) return;
    this.stopped = true;
    // Frames may still straggle in between Cancel and CancelAck: Output is
    // buffered for the caller (PROTOCOL.md §3.4) and a first-arriving Exit
    // marks the turn terminal and is cached — dropping it would leave the
    // next nextEvent() waiting on a frame that already arrived.
    const ack = await this.#host.cancelRoundTrip(null, this.requestId, (frame) => {
      if (frame.kind === KIND_OUTPUT) {
        const output = codec.decodeOutput(frame.payload);
        this.#pending.push(this.#takeOutput(output.stream, output.data));
      } else if (frame.kind === KIND_EXIT) {
        const exit = codec.decodeExit(frame.payload);
        this.terminal = true;
        this.#pending.push({ stream: -1, data: Buffer.alloc(0), code: exit.code });
      }
    });
    if (ack.payload.length !== 0) {
      throw new DecodeError('CancelAck payload must be empty');
    }
  }
}

/** Extra read budget over the client timeout + the exec turn's guest-side
 * deadline (Rust `EXEC_READ_MARGIN`); mirrors sandbox.ts's NDJSON constant —
 * duplicated rather than imported so the forkd-only split bundle (which drops
 * this module) keeps compiling. */
export const EXEC_READ_MARGIN_MS = 5_000;

export class ZbrtConnection {
  #socket: net.Socket | null = null;
  #reader: ReturnType<typeof frameReader> | undefined = undefined;
  #ready: Promise<void>;
  #timeoutMs: number = 10_000;

  constructor(host: string, port: number, timeoutMs: number);
  constructor(address: string, timeoutMs: number);
  constructor(host: string, portOrTimeout: number, timeoutMs?: number) {
    const tcp =
      timeoutMs !== undefined ? { host, port: portOrTimeout, timeout: timeoutMs } : null;
    const uds = tcp === null ? (() => {
      const parsed = parseAddress(host);
      if (!parsed.socketPath) {
        throw new ValidationError('expected uds:<path>[@<guestPort>]');
      }
      return {
        path: parsed.socketPath,
        guestPort: parsed.guestPort ?? 5000,
        timeout: portOrTimeout,
      };
    })() : null;
    this.#timeoutMs = uds !== null ? uds.timeout : tcp!.timeout;
    this.#ready = (async () => {
      if (uds !== null) {
        // 直拨 FC 的 vsock relay UDS：CONNECT 前导（非透明字节流）。
        const socket = new net.Socket();
        await new Promise<void>((resolve, reject) => {
          const timer = setTimeout(() => {
            socket.destroy();
            reject(new TransportError('guest connection timed out'));
          }, uds.timeout);
          socket.once('error', (e) => {
            clearTimeout(timer);
            reject(new TransportError(`guest connect failed: ${(e as Error).message}`));
          });
          socket.connect({ path: uds.path }, () => {
            clearTimeout(timer);
            resolve();
          });
        });
        socket.setNoDelay(true);
        socket.setTimeout(uds.timeout);
        // 读停顿合同（§3.4）：TCP 路径由 connectSocket 注册 timeout 监听，
        // uds 路径原本没有任何监听——停顿/对端死亡 = 永久 hang。
        socket.once('timeout', () => {
          socket.destroy(new TransportError('guest read timed out'));
        });
        const reply = await new Promise<string>((resolve, reject) => {
          let buf = '';
          const ondata = (d: Buffer): void => {
            buf += d.toString('utf8');
            const nl = buf.indexOf('\n');
            if (nl >= 0) {
              socket.removeListener('data', ondata);
              resolve(buf.slice(0, nl));
            }
          };
          socket.on('data', ondata);
          socket.once('error', (e) => {
            reject(new TransportError(`vsock relay handshake failed: ${(e as Error).message}`));
          });
          // 干净 FIN（无换行）只触发 close，不触发 error：不监听 close 则
          // reply 永远 pending。
          socket.once('close', () => {
            reject(new TransportError('vsock relay handshake failed: connection closed'));
          });
          socket.write(`CONNECT ${uds.guestPort}\n`);
        });
        if (!reply.startsWith('OK ')) {
          throw new TransportError(`vsock relay rejected: ${reply}`);
        }
        this.#socket = socket;
        this.#reader = frameReader(socket);
        await this.#handshake();
        return;
      }
      this.#socket = await connectSocket(tcp!.host, tcp!.port, tcp!.timeout);
      this.#reader = frameReader(this.#socket);
      // Cross-language contract: every ZBRT connection opens with a mandatory
      // Hello handshake — the session is unusable until HelloAck validates,
      // and any handshake failure is a Transport-class error.
      await this.#handshake();
    })();
    // 握手失败后 socket 已赋值但无人引用：destroy 掉，否则它带着事件
    // 循环引用活到 GC（uds relay 回 ERR 的分支同样受益）。
    this.#ready.catch(() => this.#socket?.destroy());
  }

  /** Resolves once the TCP connection is up and Hello has been acknowledged. */
  async ready(): Promise<this> {
    await this.#ready;
    return this;
  }

  newRequestId(): Buffer {
    return randomBytes(16);
  }

  /** Mandatory Hello handshake; any failure is Transport-class. */
  async #handshake(): Promise<void> {
    try {
      const id = this.newRequestId();
      await this.#writeFrame(
        new ZbrtFrame(KIND_HELLO, 0, id, codec.encodeHello(ZBRT_CLIENT_NAME, V1_CAPABILITIES)),
      );
      const frame = await this.#reader?.next();
      if (frame === null || frame === undefined) {
        throw new TransportError('connection closed by guest');
      }
      if (frame.kind !== KIND_HELLO_ACK) {
        throw new TransportError(
          `zbrt hello handshake failed: unexpected frame kind ${frame.kind} while awaiting HelloAck`,
        );
      }
      // 池借出验活依赖这条校验区分"活连接"与"串线帧"：乱入的陈旧
      // HelloAck（任意 id）必须被拒。
      await this.requireId(frame, id);
      codec.decodeHelloAck(frame.payload);
    } catch (error) {
      if (error instanceof TransportError) throw error;
      throw new TransportError(`zbrt hello handshake failed: ${(error as Error).message}`);
    }
  }

  async #writeFrame(frame: ZbrtFrame): Promise<void> {
    const socket = this.#socket;
    if (!socket) throw new TransportError('connection closed');
    await new Promise<void>((resolve, reject) => {
      // UNIFIED_API.md §7: a raw write failure must surface as Transport,
      // not a bare socket Error.
      socket.write(frame.encode(), (error) =>
        error ? reject(new TransportError(`zbrt write failed: ${error.message}`)) : resolve(),
      );
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

  async #cancelRoundTrip(
    reason: string | null,
    target16: Buffer | null,
    onStraggler?: (frame: ZbrtFrame) => void,
  ): Promise<ZbrtFrame> {
    const id = this.newRequestId();
    await this.#writeFrame(new ZbrtFrame(KIND_CANCEL, 0, id, codec.encodeCancel(reason, target16)));
    // The guest may emit straggler Output/Exit frames for the cancelled turn
    // before the CancelAck; hand them to the session (the stream stop path
    // buffers Output and caches a first-arriving Exit) and keep reading until
    // the ack instead of failing the decode (mirrors the Python reference's
    // wait-for-ack).
    while (true) {
      const frame = await this.#reader?.next();
      if (!frame) throw new TransportError('connection closed by guest');
      if (frame.kind === KIND_CANCEL_ACK) return frame;
      if (frame.kind === KIND_ERROR) throw this.errorFromFrame(frame);
      if (frame.kind === KIND_OUTPUT || frame.kind === KIND_EXIT) {
        onStraggler?.(frame);
        continue;
      }
      throw this.unexpectedFrame(frame, 'CancelAck');
    }
  }

  /** Explicit Hello round-trip; the constructor already performs the mandatory
   * handshake — this re-issues Hello for on-demand verification. */
  async hello(clientName: string): Promise<codec.HelloAck> {
    await this.#ready;
    const id = this.newRequestId();
    await this.#writeFrame(
      new ZbrtFrame(KIND_HELLO, 0, id, codec.encodeHello(clientName, V1_CAPABILITIES)),
    );
    const frame = await this.#reader?.next();
    if (frame === null || frame === undefined) throw new TransportError('connection closed by guest');
    if (frame.kind !== KIND_HELLO_ACK) {
      throw new TransportError(
        `zbrt hello handshake failed: unexpected frame kind ${frame.kind} while awaiting HelloAck`,
      );
    }
    await this.requireId(frame, id);
    return codec.decodeHelloAck(frame.payload);
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
    // 长静默 exec：读停顿预算 = 基础超时 + guest 死线 + margin，turn 结束
    // 恢复基础值——否则合法的长命令先撞客户端 socket 超时（连接被销毁），
    // 而命令还在 guest 里跑（rust/java/python 同款）。
    this.#socket?.setTimeout(this.#timeoutMs + timeoutMs + EXEC_READ_MARGIN_MS);
    try {
      const stdout: Buffer[] = [];
      const stderr: Buffer[] = [];
      let outputBytes = 0;
      while (true) {
        const frame = await this.#reader?.next();
        if (frame === null || frame === undefined) throw new TransportError('connection closed by guest');
        await this.requireId(frame, id);
        if (frame.kind === KIND_OUTPUT) {
          const output = codec.decodeOutput(frame.payload);
          // Per-turn aggregate cap: an unbounded turn would let one runaway
          // guest exhaust host memory; 16 MiB mirrors the frame payload cap.
          outputBytes += output.data.length;
          if (outputBytes > MAX_TURN_OUTPUT_BYTES) {
            throw new RemoteError(`zbrt turn output exceeded ${MAX_TURN_OUTPUT_BYTES} bytes`);
          }
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
    } finally {
      this.#socket?.setTimeout(this.#timeoutMs);
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
      cancelRoundTrip: (reason, target, onStraggler) =>
        this.#cancelRoundTrip(reason, target, onStraggler),
    };
    return new ZbrtStreamSession(host, id);
  }

  close(): void {
    this.#socket?.end();
    this.#socket?.destroy();
  }
}
