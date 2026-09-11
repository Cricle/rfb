/**
 * One connected sandbox. Guest operations run over the NDJSON or the ZBRT
 * transport — same names, same result shapes (UNIFIED_API.md §4–§5).
 */
import net from 'node:net';
import type { RfbError } from './errors.js';
import { DecodeError, RemoteError, TransportError, ValidationError } from './errors.js';
import { parseAddress, request as ndjsonRequest } from './ndjson.js';
import type { NdjsonExchange } from './ndjson.js';
import * as validation from './validation.js';
import { ZbrtConnection } from './zbrt-connection.js';


export const TRANSPORT_NDJSON = 'ndjson';
export const TRANSPORT_ZBRT = 'zbrt';

/** Unified result of a guest exec/eval. */
export interface ExecResult {
  exitCode: number;
  stdout: Buffer;
  stderr: Buffer;
  readonly stdoutText: string;
  readonly stderrText: string;
  timedOut: boolean;
}

export interface ExecOptions {
  cwd?: string;
  timeoutS?: number;
  /** Delivered over ZBRT only — the NDJSON wire has no exec stdin channel. */
  stdin?: Uint8Array;
}

export interface EvalOptions {
  cwd?: string;
  timeoutS?: number;
}

export interface ReadOptions {
  offset?: number;
  maxBytes?: number;
}

export interface WriteOptions {
  append?: boolean;
}

export interface StreamOptions {
  cwd?: string;
  pty?: boolean | null;
  env?: Record<string, string> | null;
}

export interface FileRead {
  data: Buffer;
  truncated: boolean;
  totalBytes: number | null;
}

export interface DirEntry {
  name: string;
  isDir: boolean;
  size: number | null;
}

export interface GrepMatch {
  path: string;
  line: number | null;
  column: number | null;
  text: string;
}

export type StreamEventKind = 'started' | 'stdout' | 'stderr' | 'exit';

export interface StreamEvent {
  kind: StreamEventKind;
  data: Buffer;
  code: number | null;
}

/** Interactive stream handle (UNIFIED_API.md §5). */
export interface GuestStream {
  nextEvent(): Promise<StreamEvent | null>;
  sendInput(text: string): Promise<void>;
  stop(): Promise<void>;
}

function statusCode(value: Record<string, unknown>, fallback: number): number {
  if (typeof value.exit_code === 'number') return value.exit_code;
  if (typeof value.status === 'number') return value.status;
  return fallback;
}

/** current-key-first lookup with the legacy agent key as fallback. */
function firstOf(value: Record<string, unknown>, current: string, legacy: string): unknown {
  return value?.[current] ?? value?.[legacy];
}

function valueBytes(value: unknown): Buffer {
  if (value === undefined || value === null) return Buffer.alloc(0);
  if (typeof value === 'string') return Buffer.from(value, 'utf8');
  if (Array.isArray(value)) return Buffer.from(value as number[]);
  throw new DecodeError('expected a byte array or string');
}

function fileReadFromJson(payload: Buffer): FileRead {
  const node = JSON.parse(payload.toString('utf8')) as Record<string, unknown>;
  const data = Array.isArray(node.data) ? Buffer.from(node.data as number[]) : Buffer.alloc(0);
  return {
    data,
    truncated: node.truncated === true,
    totalBytes: typeof node.total_bytes === 'number' ? node.total_bytes : null,
  };
}

export interface SandboxInfo {
  id: string;
  snapshot_tag?: string;
  guest_addr?: string;
  [key: string]: unknown;
}

export class Sandbox {
  public readonly info: SandboxInfo;
  public readonly id: string;
  public readonly snapshotTag: string;
  public readonly guestAddr: string;
  public readonly transport: string;
  readonly #client: { deleteSandbox(id: string): Promise<void>; };
  readonly #guestTimeoutMs: number;

  constructor(info: SandboxInfo, client: { deleteSandbox(id: string): Promise<void> }, transport: string, guestTimeoutMs = 10_000) {
    this.info = info;
    this.id = String(info.id);
    this.snapshotTag = String(info.snapshot_tag ?? '');
    this.guestAddr = String(info.guest_addr ?? '');
    this.transport = transport;
    this.#client = client;
    this.#guestTimeoutMs = guestTimeoutMs;
  }

  /** Guest health check: true only when pong=true. */
  async ping(): Promise<boolean> {
    if (this.transport === TRANSPORT_ZBRT) {
      const conn = await this.#zbrt();
      try {
        return (await conn.health()).healthy;
      } finally {
        conn.close();
      }
    }
    const { last } = await this.#guestRequest({ action: 'ping' });
    return last?.pong === true;
  }

  async exec(args: readonly string[], options: ExecOptions = {}): Promise<ExecResult> {
    const { cwd = '/workspace', timeoutS = 60, stdin = null } = options;
    validation.argv(args);
    validation.filePath(cwd);
    validation.timeoutS(timeoutS);
    if (this.transport === TRANSPORT_ZBRT) {
      const conn = await this.#zbrt();
      try {
        const exec = await conn.execute(
          args,
          cwd,
          stdin ?? Buffer.alloc(0),
          Math.ceil(timeoutS * 1000),
        );
        return this.#execResult(exec.code, exec.stdout, exec.stderr, exec.timedOut);
      } finally {
        conn.close();
      }
    }
    // Rust baseline: the NDJSON exec wire contract has no stdin channel;
    // non-empty stdin is silently dropped (delivered only over ZBRT).
    const { last } = await this.#guestRequest({
      action: 'exec',
      cwd,
      timeout: Math.ceil(timeoutS),
      args,
    });
    return this.#execResult(statusCode(last, -1), valueBytes(firstOf(last, 'stdout', 'out')), valueBytes(firstOf(last, 'stderr', 'err')), last.timed_out === true);
  }

  /** Evaluate a code snippet; output maps to stdout. */
  async eval(code: string, options: EvalOptions = {}): Promise<ExecResult> {
    validation.evalCode(code);
    if (options.cwd !== undefined && options.cwd !== null) validation.filePath(options.cwd);
    if (options.timeoutS !== undefined && options.timeoutS !== null) validation.timeoutS(options.timeoutS);
    if (this.transport === TRANSPORT_ZBRT) {
      const conn = await this.#zbrt();
      try {
        const exec = await conn.execute(
          ['eval', code],
          options.cwd ?? undefined,
          Buffer.alloc(0),
          options.timeoutS === undefined || options.timeoutS === null ? 0 : Math.ceil(options.timeoutS * 1000),
        );
        return this.#execResult(exec.code, exec.stdout, Buffer.alloc(0), exec.timedOut);
      } finally {
        conn.close();
      }
    }
    const action: Record<string, unknown> = { action: 'eval', code };
    if (options.cwd !== undefined && options.cwd !== null) action.cwd = options.cwd;
    if (options.timeoutS !== undefined && options.timeoutS !== null) action.timeout = Math.ceil(options.timeoutS);
    const { last } = await this.#guestRequest(action);
    const out = valueBytes(firstOf(last, 'out', 'output'));
    return this.#execResult(statusCode(last, 0), out, Buffer.alloc(0), last.timed_out === true);
  }

  /** List directory entries (default path "."). */
  async ls(path = '.'): Promise<DirEntry[]> {
    validation.fsPath(path);
    const { last } = await this.#fsRequest(1, path, {
      max_results: validation.MAX_GUEST_RESULTS,
    });
    this.#checkResultSize(last);
    const entries = last.entries;
    if (!Array.isArray(entries)) {
      throw new DecodeError('guest response is missing entries');
    }
    return entries.map((entry: Record<string, unknown>) => ({
      name: String(entry.name ?? ''),
      isDir: entry.is_dir === true,
      size: typeof entry.size === 'number' ? entry.size : null,
    }));
  }

  /** Find workspace paths matching a glob-ish pattern. */
  async find(path: string, pattern: string): Promise<string[]> {
    validation.fsPath(path);
    validation.pattern(pattern);
    const { last } = await this.#fsRequest(2, path, {
      max_results: validation.MAX_GUEST_RESULTS,
      pattern,
    });
    this.#checkResultSize(last);
    const matches = last.matches;
    if (!Array.isArray(matches)) {
      throw new DecodeError('guest response is missing matches');
    }
    return matches.map((match: unknown) => String(match));
  }

  /** Grep file contents; matches carry path/line/column/text. */
  async grep(path: string, pattern: string): Promise<GrepMatch[]> {
    validation.fsPath(path);
    validation.pattern(pattern);
    const { last } = await this.#fsRequest(3, path, {
      max_results: validation.MAX_GUEST_RESULTS,
      pattern,
      max_bytes: validation.MAX_GUEST_RESULT_BYTES,
    });
    this.#checkResultSize(last);
    const matches = last.matches;
    if (!Array.isArray(matches)) {
      throw new DecodeError('guest response is missing matches');
    }
    return matches.map((match: Record<string, unknown>) => ({
      path: String(match.path ?? ''),
      line: typeof match.line === 'number' ? match.line : null,
      column: typeof match.column === 'number' ? match.column : null,
      text: String(match.text ?? ''),
    }));
  }

  /** Read a guest file: `{data, truncated, totalBytes}`. */
  async read(path: string, options: ReadOptions = {}): Promise<FileRead> {
    validation.filePath(path);
    const maxBytes = options.maxBytes ?? validation.MAX_GUEST_RESULT_BYTES;
    validation.limit(maxBytes, validation.MAX_GUEST_RESULT_BYTES);
    if (this.transport === TRANSPORT_ZBRT) {
      const conn = await this.#zbrt();
      try {
        // PROTOCOL.md §3.3: the Fs data object always carries the keys, with
        // null for absent optionals.
        const payload = await conn.fs(4, path, Buffer.from(JSON.stringify({ offset: options.offset ?? null, max_bytes: maxBytes }), 'utf8'));
        return fileReadFromJson(payload);
      } finally {
        conn.close();
      }
    }
    const action: Record<string, unknown> = { action: 'read', path };
    if (options.offset !== undefined && options.offset !== null) action.offset = options.offset;
    if (options.maxBytes !== undefined) action.max_bytes = options.maxBytes;
    const { last } = await this.#guestRequest(action);
    const data = valueBytes(last.data);
    return {
      data,
      truncated: last.truncated === true,
      totalBytes: typeof last.total_bytes === 'number' ? last.total_bytes : null,
    };
  }

  /** Write (or append to) a guest file; returns bytes_written. */
  async write(path: string, data: Uint8Array | string, options: WriteOptions = {}): Promise<number> {
    validation.filePath(path);
    const bytes = typeof data === 'string' ? Buffer.from(data, 'utf8') : Buffer.from(data);
    validation.payloadSize(bytes.length, validation.MAX_GUEST_RESULT_BYTES);
    if (this.transport === TRANSPORT_ZBRT) {
      const conn = await this.#zbrt();
      try {
        const payload = await conn.fs(5, path, Buffer.from(JSON.stringify({ data: [...bytes], append: options.append ?? false, mode: null }), 'utf8'));
        const last = JSON.parse(payload.toString('utf8')) as Record<string, unknown>;
        if (typeof last.bytes_written !== 'number') {
          throw new DecodeError('guest response is missing bytes_written');
        }
        return last.bytes_written;
      } finally {
        conn.close();
      }
    }
    const { last } = await this.#guestRequest({
      action: 'write',
      path,
      data: [...bytes],
      append: options.append ?? false,
    });
    if (typeof last.bytes_written !== 'number') {
      throw new DecodeError('guest response is missing bytes_written');
    }
    return last.bytes_written;
  }

  /** Interactive stream over the sandbox transport. */
  async stream(args: readonly string[], options: StreamOptions = {}): Promise<GuestStream> {
    validation.argv(args);
    const cwd = options.cwd ?? '/workspace';
    validation.filePath(cwd);
    if (this.transport === TRANSPORT_ZBRT) {
      // Fail closed: ZBRT v1 has neither a pty nor an env channel.
      if (options.pty === true) {
        throw new ValidationError('pty is not supported over the ZBRT transport');
      }
      if (options.env !== null && options.env !== undefined && Object.keys(options.env).length > 0) {
        throw new ValidationError('env is not supported over the ZBRT transport');
      }
      const conn = await this.#zbrt();
      const session = await conn.openStreamSession(args, cwd, Buffer.alloc(0), 0);
      return new ZbrtGuestStream(conn, session);
    }
    return new NdjsonGuestStream(this.guestAddr, args, cwd, this.#guestTimeoutMs);
  }

  /** Delete the sandbox via the controller (2xx and 404 are both success). */
  async delete(): Promise<void> {
    await this.#client.deleteSandbox(this.id);
  }

  async #zbrt(): Promise<ZbrtConnection> {
    const { host, port } = parseAddress(this.guestAddr);
    const conn = new ZbrtConnection(host, port, this.#guestTimeoutMs);
    await conn.ready();
    return conn;
  }

  async #guestRequest(action: Record<string, unknown>): Promise<NdjsonExchange> {
    const { host, port } = parseAddress(this.guestAddr);
    return ndjsonRequest(host, port, this.#guestTimeoutMs, action);
  }

  async #fsRequest(op: number, path: string, args: Record<string, unknown>): Promise<NdjsonExchange> {
    const action = { action: FS_ACTIONS[op], path, ...args };
    return this.#guestRequest(action);
  }

  #checkResultSize(last: Record<string, unknown>): void {
    if (Buffer.byteLength(JSON.stringify(last), 'utf8') > validation.MAX_GUEST_RESULT_BYTES) {
      throw new TransportError('guest response exceeded limit');
    }
    const results = last.results;
    if (Array.isArray(results) && results.length > validation.MAX_GUEST_RESULTS) {
      throw new RemoteError('guest result limit exceeded');
    }
  }

  #execResult(exitCode: number, stdout: Buffer, stderr: Buffer, timedOut: boolean): ExecResult {
    return {
      exitCode,
      stdout,
      stderr,
      stdoutText: stdout.toString('utf8').trimEnd(),
      stderrText: stderr.toString('utf8'),
      timedOut,
    };
  }
}

const FS_ACTIONS: readonly string[] = ['ls', 'find', 'grep', 'read', 'write'];

/** NDJSON interactive stream: started → out/err → exit, with send_input/stop. */
class NdjsonGuestStream implements GuestStream {
  #socket: net.Socket | null = null;
  #buffer = Buffer.alloc(0);
  #pending: StreamEvent[] = [];
  // Waiter queue (not a single slot): concurrent nextEvent callers must all
  // be woken or all but one hang forever.
  #waiters: (() => void)[] = [];
  #closed = false;
  #failure: RfbError | null = null;
  readonly #address: string;
  readonly #args: readonly string[];
  readonly #cwd: string;
  readonly #timeoutMs: number;

  constructor(address: string, args: readonly string[], cwd: string, timeoutMs: number) {
    this.#address = address;
    this.#args = args;
    this.#cwd = cwd;
    this.#timeoutMs = timeoutMs;
  }

  /** Next event: {kind, data, code} — null after a clean close. */
  async nextEvent(): Promise<StreamEvent | null> {
    if (this.#socket === null) await this.#connect();
    while (true) {
      if (this.#pending.length > 0) return this.#pending.shift() as StreamEvent;
      if (this.#failure !== null) throw this.#failure;
      if (this.#closed) return null;
      await new Promise<void>((resolve) => {
        this.#waiters.push(resolve);
      });
    }
  }

  /** Send one stdin payload to the child (NDJSON only). */
  async sendInput(text: string): Promise<void> {
    if (this.#socket === null) await this.#connect();
    this.#socket?.write(Buffer.from(JSON.stringify({ in: text }) + '\n', 'utf8'));
  }

  /** Idempotent stop: terminates the child; the exit frame follows. */
  async stop(): Promise<void> {
    if (this.#socket === null) await this.#connect();
    this.#socket?.write(Buffer.from(JSON.stringify({ action: 'stop' }) + '\n', 'utf8'));
  }

  async #connect(): Promise<void> {
    const { host, port } = parseAddress(this.#address);
    const socket = net.createConnection({ host, port });
    socket.setTimeout(this.#timeoutMs);
    socket.setNoDelay(true);
    socket.on('error', (error) => {
      this.#fail(new TransportError(`stream error: ${error.message}`));
      socket.destroy();
    });
    socket.on('timeout', () => {
      this.#fail(new TransportError('stream timed out'));
      socket.destroy();
    });
    socket.on('data', (chunk: Buffer) => {
      this.#buffer = Buffer.concat([this.#buffer, chunk]);
      this.#pump();
    });
    socket.on('close', () => {
      this.#closed = true;
      this.#pump();
    });
    socket.write(Buffer.from(JSON.stringify({ action: 'stream', args: this.#args, cwd: this.#cwd }) + '\n', 'utf8'));
  }

  #pump(): void {
    while (true) {
      const nl = this.#buffer.indexOf('\n');
      if (nl < 0) break;
      let raw = this.#buffer.subarray(0, nl);
      this.#buffer = this.#buffer.subarray(nl + 1);
      while (raw.length > 0 && (raw[raw.length - 1] === 0x0a || raw[raw.length - 1] === 0x0d)) {
        raw = raw.subarray(0, raw.length - 1);
      }
      if (raw.length === 0) continue;
      let value: Record<string, unknown>;
      try {
        value = JSON.parse(raw.toString('utf8')) as Record<string, unknown>;
      } catch (error) {
        this.#fail(new DecodeError(`invalid stream JSON: ${(error as Error).message}`));
        return;
      }
      const record = value as Record<string, unknown> & { error?: unknown; stream?: unknown; started?: unknown; out?: unknown; err?: unknown; exit_code?: unknown; done?: unknown };
      if (value && typeof value.error === 'string') {
        this.#fail(new RemoteError(value.error));
        return;
      }
      if (record.stream === 'started' || record.started === true) {
        this.#pending.push({ kind: 'started', data: Buffer.alloc(0), code: null });
      } else if (typeof record.out === 'string' || Array.isArray(record.out)) {
        this.#pending.push({ kind: 'stdout', data: Buffer.from(record.out as string | number[]), code: null });
      } else if (typeof record.err === 'string' || Array.isArray(record.err)) {
        this.#pending.push({ kind: 'stderr', data: Buffer.from(record.err as string | number[]), code: null });
      } else if (typeof record.exit_code === 'number' || record.exit_code === null) {
        this.#pending.push({
          kind: 'exit',
          data: Buffer.alloc(0),
          code: typeof record.exit_code === 'number' ? record.exit_code : null,
        });
      } else if (record.done === true) {
        this.#pending.push({ kind: 'exit', data: Buffer.alloc(0), code: null });
      }
      this.#wake();
    }
  }

  #fail(error: RfbError): void {
    if (this.#failure === null) this.#failure = error;
    this.#wake();
  }

  #wake(): void {
    const ready = this.#waiters.splice(0);
    for (const resolve of ready) resolve();
  }
}

/** ZBRT interactive stream: Output frames → stdout/stderr, Exit → exit. */
class ZbrtGuestStream implements GuestStream {
  #conn: ZbrtConnection;
  #session: { nextEvent(): Promise<{ stream: number; data: Buffer; code: number | null } | null>; stop(): Promise<void> };

  constructor(conn: ZbrtConnection, session: { nextEvent(): Promise<{ stream: number; data: Buffer; code: number | null } | null>; stop(): Promise<void> }) {
    this.#conn = conn;
    this.#session = session;
  }

  async nextEvent(): Promise<StreamEvent | null> {
    const event = await this.#session.nextEvent();
    if (event === null) {
      // Session over: release the transport socket instead of leaking it
      // until GC/process exit.
      this.#conn.close();
      return null;
    }
    if (event.code !== null) {
      this.#conn.close();
      return { kind: 'exit', data: Buffer.alloc(0), code: event.code };
    }
    return {
      kind: event.stream === 0 ? 'stdout' : 'stderr',
      data: event.data,
      code: null,
    };
  }

  /** ZBRT v1 has no stdin channel — sends raise TransportError. */
  async sendInput(_text: string): Promise<void> {
    throw new TransportError('zbrt v1 has no input channel');
  }

  /** Idempotent stop: Cancel targeting this turn, expecting CancelAck. */
  async stop(): Promise<void> {
    await this.#session.stop();
  }
}
