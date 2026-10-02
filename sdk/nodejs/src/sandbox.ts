/**
 * One connected sandbox. Guest operations run over the NDJSON or the ZBRT
 * transport — same names, same result shapes (UNIFIED_API.md §4–§5).
 */
import net from 'node:net';
import type { RfbError } from './errors.js';
import { DecodeError, RemoteError, TransportError, ValidationError } from './errors.js';
import { parseAddress, NdjsonPool, agentAuthToken } from './ndjson.js';
import type { NdjsonExchange } from './ndjson.js';
import * as validation from './validation.js';
import { ZbrtConnection } from './zbrt-connection.js';


export const TRANSPORT_NDJSON = 'ndjson';
export const TRANSPORT_ZBRT = 'zbrt';
/** Default ZBRT bridge TCP endpoint (RFB_ZBRT_TCP default). */
export const DEFAULT_ZBRT_TCP = '127.0.0.1:15000';

/** Extra read budget over the client + exec deadline (Rust `EXEC_READ_MARGIN`). */
const EXEC_READ_MARGIN_MS = 5_000;
const MAX_U32 = 0xffff_ffff;

/** exec/eval timeout on the wire: whole seconds, ceil, minimum 1 (PROTOCOL.md §2.2). */
function timeoutSeconds(timeoutS: number): number {
  return Math.max(1, Math.ceil(timeoutS));
}

/**
 * ZBRT deadline in ms: whole seconds ×1000, clamped to u32::MAX (Rust
 * `facade.rs` GuestOps::exec / ::eval ZBRT branch).
 */
function zbrtTimeoutMs(timeoutS: number): number {
  return Math.min(timeoutSeconds(timeoutS) * 1000, MAX_U32);
}

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

/** Interactive stream handle (§5). Consumed with `await for` over
 * `nextEvent()`, or event-by-event; `null` marks a clean close. */
export interface GuestStream {
  /**
   * Next event, or `null` once the stream closed cleanly (after the terminal
   * exit, or when the peer disconnected). Over ZBRT the first event is a
   * client-synthesized `started` (the wire has no started frame).
   *
   * @throws {RemoteError} Guest error line / Error frame / input after end.
   * @throws {TransportError} Connection failure or read stall.
   * @throws {DecodeError} Malformed frame or line.
   */
  nextEvent(): Promise<StreamEvent | null>;
  /**
   * Send one stdin payload (NDJSON only; ZBRT v1 has no input channel →
   * {@link RemoteError}). After a terminal exit or `stop()` → {@link RemoteError}.
   */
  sendInput(text: string): Promise<void>;
  /** Idempotently ask the guest to terminate the stream; the exit event follows. */
  stop(): Promise<void>;
}

/**
 * Status lookup, current key first (UNIFIED_API.md §4): exec answers with
 * `exit_code`, eval with `status` (PROTOCOL.md §2.4) — the caller names the
 * pair in precedence order.
 */
function statusCode(value: Record<string, unknown>, fallback: number, current = 'exit_code', legacy = 'status'): number {
  if (typeof value[current] === 'number') return value[current] as number;
  if (typeof value[legacy] === 'number') return value[legacy] as number;
  return fallback;
}

/** current-key-first lookup with the legacy agent key as fallback. */
function firstOf(value: Record<string, unknown>, current: string, legacy: string): unknown {
  return value?.[current] ?? value?.[legacy];
}

function valueBytes(value: unknown): Buffer {
  if (value === undefined || value === null) return Buffer.alloc(0);
  if (typeof value === 'string') return Buffer.from(value, 'utf8');
  if (Array.isArray(value)) {
    // Reject anything that is not a u8 instead of Buffer.from's silent coercion
    // (matches the Rust baseline's strict decode).
    const bytes = Buffer.alloc(value.length);
    for (let i = 0; i < value.length; i += 1) {
      const item = value[i];
      if (typeof item !== 'number' || !Number.isInteger(item) || item < 0 || item > 0xff) {
        throw new DecodeError('expected a byte array or string');
      }
      bytes[i] = item;
    }
    return bytes;
  }
  throw new DecodeError('expected a byte array or string');
}

/** Current-key-first lookup that distinguishes "absent" from "explicit null". */
function streamBytes(
  record: Record<string, unknown>,
  current: string,
  legacy: string,
): Buffer | null {
  const key = current in record ? current : legacy in record ? legacy : null;
  if (key === null) return null;
  return valueBytes(record[key]);
}

function fileReadFromJson(payload: Buffer): FileRead {
  const node = JSON.parse(payload.toString('utf8')) as Record<string, unknown>;
  return {
    data: valueBytes(node.data),
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
  /** exec 温连接池：已 Hello 的空闲连接，借还复用（空闲 >1s 才验活）。 */
  #zbrtExecPool: { conn: ZbrtConnection; lastUsed: number }[] = [];
  /** NDJSON 温连接池（forkd 生产路径的同款借还语义）。 */
  #ndjsonPool: NdjsonPool | null = null;
  readonly #guestTimeoutMs: number;

  constructor(info: SandboxInfo, client: { deleteSandbox(id: string): Promise<void> }, transport: string, guestTimeoutMs = 10_000) {
    this.info = info;
    this.id = String(info.id);
    this.snapshotTag = String(info.snapshot_tag ?? '');
    this.guestAddr = String(info.guest_addr ?? '');
    this.transport = transport;
    if (transport !== TRANSPORT_ZBRT && String(info.guest_addr ?? '').startsWith('uds:')) {
      // uds: 是 FC vsock relay 的 ZBRT 直拨形态；NDJSON 只走 TCP（TAP 网段），
      // 留到请求期才失败会把错误埋在深处。
      throw new ValidationError(
        'uds: guest addresses require the zbrt transport',
      );
    }
    this.#client = client;
    this.#guestTimeoutMs = guestTimeoutMs;
  }

  /** Creation time from the controller, or null when absent (§4 readonly props). */
  get createdAtUnix(): number | null {
    return typeof this.info.created_at_unix === 'number' ? this.info.created_at_unix : null;
  }

  /**
   * Guest health check (§4): `true` only when the agent answers `pong=true`
   * (NDJSON ignores the extra `healthy`/`protocol_version` keys; ZBRT uses
   * the HealthAck `healthy` flag).
   *
   * @returns `true` when the guest answers healthy.
   * @throws {TransportError} Connection failure, read stall or timeout.
   * @throws {RemoteError} The agent reported an error.
   * @throws {DecodeError} Malformed response.
   */
  async ping(): Promise<boolean> {
    if (this.transport === TRANSPORT_ZBRT) {
      return (await this.#withExecConn((conn) => conn.health())).healthy;
    }
    const { last } = await this.#guestRequest({ action: 'ping' });
    return last?.pong === true;
  }

  /**
   * Run one command in the guest (§4). All validation is local and fail
   * closed: empty/non-string argv, bad cwd or timeout raise
   * {@link ValidationError} before any frame; non-empty `stdin` over NDJSON
   * (no stdin channel on that wire) and ZBRT argc > 255 / oversized stdin are
   * rejected before a single frame — for ZBRT before the TCP connect itself.
   * A missing or non-integer `exit_code` decodes to `-1`; legacy agents'
   * `out`/`err` keys are accepted with the current keys taking precedence.
   *
   * @param args Command + arguments (non-empty string array).
   * @param options Optional: `cwd` (default `/workspace`), `timeoutS`
   *   (default 60), `stdin` (bytes, ZBRT only).
   * @returns The unified exec result (stdout/stderr bytes + `stdoutText`/
   *   `stderrText` UTF-8 replace decoding, `timedOut`).
   * @throws {ValidationError} Local pre-send validation failed.
   * @throws {TransportError} Connection failure, read stall or timeout.
   * @throws {RemoteError} Guest error line / Error frame / output over 16 MiB.
   * @throws {DecodeError} Malformed response.
   */
  async exec(args: readonly string[], options: ExecOptions = {}): Promise<ExecResult> {
    const { cwd = '/workspace', timeoutS = 60, stdin = null } = options;
    validation.argv(args);
    validation.filePath(cwd);
    validation.timeoutS(timeoutS);
    if (this.transport === TRANSPORT_ZBRT) {
      // §4: argc and the stdin payload are rejected locally, BEFORE any
      // connection is opened (fail closed, zero frames — not even TCP).
      validation.zbrtArgs(args);
      const stdinBytes = stdin ?? Buffer.alloc(0);
      validation.payloadSize(stdinBytes.byteLength, validation.MAX_ZBRT_PAYLOAD_BYTES);
      const exec = await this.#withExecConn(
        (conn) => conn.execute(args, cwd, stdinBytes, zbrtTimeoutMs(timeoutS)));
      return this.#execResult(exec.code, exec.stdout, exec.stderr, exec.timedOut);
    }
    // The NDJSON exec wire contract has no stdin channel: non-empty stdin
    // would run the command WITHOUT its input, so fail closed (delivered
    // only over ZBRT; mirrors the Rust baseline).
    if (stdin !== null && stdin.byteLength > 0) {
      throw new ValidationError('stdin is only supported over the ZBRT transport');
    }
    const seconds = timeoutSeconds(timeoutS);
    const { last } = await this.#guestRequest(
      {
        action: 'exec',
        cwd,
        timeout: seconds,
        args,
      },
      // The read budget must cover the guest's own deadline, or a legitimately
      // long exec would surface the client timeout instead of the guest's
      // (Rust forkd/guest.rs EXEC_READ_MARGIN, Python _guest.py).
      this.#guestTimeoutMs + seconds * 1000 + EXEC_READ_MARGIN_MS,
    );
    return this.#execResult(statusCode(last, -1), valueBytes(firstOf(last, 'stdout', 'out')), valueBytes(firstOf(last, 'stderr', 'err')), last.timed_out === true);
  }

  /**
   * Evaluate a code snippet in the guest (§4): the agent's output maps to
   * `stdout`, `stderr` is always empty and the exit code comes from `status`
   * (legacy `exit_code` alias; default 0 when missing/non-integer). Over ZBRT
   * this fails closed with {@link ValidationError} before any frame — ZBRT v1
   * has no eval opcode.
   *
   * @param code Code to evaluate (non-empty after trim, ≤ 1 MiB).
   * @param options Optional: `cwd` (guest default when null), `timeoutS`
   *   (no deadline when null).
   * @returns The unified result with `stderr` empty.
   * @throws {ValidationError} Empty/oversized code, bad cwd/timeout, or eval
   *   over ZBRT.
   * @throws {TransportError} Connection failure, read stall or timeout.
   * @throws {RemoteError} Guest error line.
   * @throws {DecodeError} Malformed response.
   */
  async eval(code: string, options: EvalOptions = {}): Promise<ExecResult> {
    validation.evalCode(code);
    if (options.cwd !== undefined && options.cwd !== null) validation.filePath(options.cwd);
    if (options.timeoutS !== undefined && options.timeoutS !== null) validation.timeoutS(options.timeoutS);
    if (this.transport === TRANSPORT_ZBRT) {
      // ZBRT v1 has no eval opcode and the reference guest maps Execute
      // verbatim onto `exec` — fail closed instead of running a literal
      // `eval <code>` command (mirrors the Rust facade).
      throw new ValidationError('eval is not supported over the ZBRT transport');
    }
    const action: Record<string, unknown> = { action: 'eval', code };
    if (options.cwd !== undefined && options.cwd !== null) action.cwd = options.cwd;
    const evalSeconds =
      options.timeoutS !== undefined && options.timeoutS !== null
        ? timeoutSeconds(options.timeoutS)
        : null;
    if (evalSeconds !== null) action.timeout = evalSeconds;
    const { last } = await this.#guestRequest(
      action,
      evalSeconds === null
        ? this.#guestTimeoutMs
        : this.#guestTimeoutMs + evalSeconds * 1000 + EXEC_READ_MARGIN_MS,
    );
    const out = valueBytes(firstOf(last, 'output', 'out'));
    // Rust baseline (ndjson::eval_result): `status` is the agent's current
    // eval key, `exit_code` the legacy alias.
    return this.#execResult(
      statusCode(last, 0, 'status', 'exit_code'),
      out,
      Buffer.alloc(0),
      last.timed_out === true,
    );
  }

  /**
   * List directory entries under `path` (§4; default `"."`, max 1000 entries).
   *
   * @param path Guest fs path (relative or under `/workspace`).
   * @returns Entries with `name`/`isDir`/`size`.
   * @throws {ValidationError} Invalid path.
   * @throws {TransportError} Connection failure, read stall or timeout.
   * @throws {RemoteError} Guest error line / Error frame.
   * @throws {DecodeError} Malformed or missing `entries`.
   */
  async ls(path = '.'): Promise<DirEntry[]> {
    validation.fsPath(path);
    if (this.transport === TRANSPORT_ZBRT) {
      const payload = await this.#withExecConn(
        (conn) => conn.fs(1, path, Buffer.from(JSON.stringify({ max_results: validation.MAX_GUEST_RESULTS }), 'utf8')));
      return this.#parseLs(this.#fsResultJson(payload));
    }
    const { last } = await this.#fsRequest(1, path, {
      max_results: validation.MAX_GUEST_RESULTS,
    });
    return this.#parseLs(last);
  }

  /**
   * Find workspace paths matching a glob-ish pattern (§4; max 1000 results).
   *
   * @param path Guest fs path (relative or under `/workspace`).
   * @param pattern Non-empty, NUL-free pattern ≤ 1024 bytes.
   * @returns Matching guest paths as strings.
   * @throws {ValidationError} Invalid path or pattern.
   * @throws {TransportError} Connection failure, read stall or timeout.
   * @throws {RemoteError} Guest error line / Error frame.
   * @throws {DecodeError} Malformed or missing `matches`.
   */
  async find(path: string, pattern: string): Promise<string[]> {
    validation.fsPath(path);
    validation.pattern(pattern);
    if (this.transport === TRANSPORT_ZBRT) {
      const payload = await this.#withExecConn(
        (conn) => conn.fs(2, path, Buffer.from(JSON.stringify({ pattern, max_results: validation.MAX_GUEST_RESULTS }), 'utf8')));
      return this.#parseFind(this.#fsResultJson(payload));
    }
    const { last } = await this.#fsRequest(2, path, {
      max_results: validation.MAX_GUEST_RESULTS,
      pattern,
    });
    return this.#parseFind(last);
  }

  /**
   * Grep file contents (§4; max 1000 matches / 50 KiB of results).
   *
   * @param path Guest fs path (relative or under `/workspace`).
   * @param pattern Non-empty, NUL-free pattern ≤ 1024 bytes.
   * @returns Matches with `path`/`line`/`column`/`text`.
   * @throws {ValidationError} Invalid path or pattern.
   * @throws {TransportError} Connection failure, read stall or timeout.
   * @throws {RemoteError} Guest error line / Error frame.
   * @throws {DecodeError} Malformed or missing `matches`.
   */
  async grep(path: string, pattern: string): Promise<GrepMatch[]> {
    validation.fsPath(path);
    validation.pattern(pattern);
    if (this.transport === TRANSPORT_ZBRT) {
      const payload = await this.#withExecConn(
        (conn) => conn.fs(3, path, Buffer.from(JSON.stringify({ pattern, max_results: validation.MAX_GUEST_RESULTS, max_bytes: validation.MAX_GUEST_RESULT_BYTES }), 'utf8')));
      return this.#parseGrep(this.#fsResultJson(payload));
    }
    const { last } = await this.#fsRequest(3, path, {
      max_results: validation.MAX_GUEST_RESULTS,
      pattern,
      max_bytes: validation.MAX_GUEST_RESULT_BYTES,
    });
    return this.#parseGrep(last);
  }

  /**
   * Read a guest file (§4): `{data, truncated, totalBytes}`. `maxBytes` must
   * be within `1..51200` when given; absent `offset`/`maxBytes` ride the wire
   * as null (ZBRT) or omitted keys (NDJSON), letting the guest apply its own
   * default.
   *
   * @param path Guest file path (relative or absolute, non-escaping).
   * @param options Optional: `offset`, `maxBytes` (1..51200).
   * @returns The file bytes plus truncation info.
   * @throws {ValidationError} Invalid path or out-of-range `maxBytes`.
   * @throws {TransportError} Connection failure, read stall or timeout.
   * @throws {RemoteError} Guest error line / Error frame.
   * @throws {DecodeError} Malformed response.
   */
  async read(path: string, options: ReadOptions = {}): Promise<FileRead> {
    validation.filePath(path);
    // No implicit cap: absent max_bytes rides the wire as null (PROTOCOL.md
    // §3.3) over ZBRT and as an omitted key over NDJSON (§2.2), letting the
    // guest apply its own default — mirroring the other SDKs.
    const maxBytes = options.maxBytes ?? null;
    if (maxBytes !== null) validation.limit(maxBytes, validation.MAX_GUEST_RESULT_BYTES);
    if (this.transport === TRANSPORT_ZBRT) {
      // PROTOCOL.md §3.3: the Fs data object always carries the keys, with
      // null for absent optionals.
      const payload = await this.#withExecConn(
        (conn) => conn.fs(4, path, Buffer.from(JSON.stringify({ offset: options.offset ?? null, max_bytes: maxBytes }), 'utf8')));
      return fileReadFromJson(payload);
    }
    const action: Record<string, unknown> = { action: 'read', path };
    if (options.offset !== undefined && options.offset !== null) action.offset = options.offset;
    if (maxBytes !== null) action.max_bytes = maxBytes;
    const { last } = await this.#guestRequest(action);
    const data = valueBytes(last.data);
    return {
      data,
      truncated: last.truncated === true,
      totalBytes: typeof last.total_bytes === 'number' ? last.total_bytes : null,
    };
  }

  /**
   * Write (or append to) a guest file (§4): payload ≤ 51200 bytes; returns
   * the guest's `bytes_written` (missing key → {@link DecodeError}).
   *
   * @param path Guest file path (relative or absolute, non-escaping).
   * @param data Bytes (or UTF-8 string) to write.
   * @param options Optional: `append` (default false).
   * @returns Bytes written as reported by the guest.
   * @throws {ValidationError} Invalid path or oversized payload.
   * @throws {TransportError} Connection failure, read stall or timeout.
   * @throws {RemoteError} Guest error line / Error frame.
   * @throws {DecodeError} Malformed response / missing `bytes_written`.
   */
  async write(path: string, data: Uint8Array | string, options: WriteOptions = {}): Promise<number> {
    validation.filePath(path);
    const bytes = typeof data === 'string' ? Buffer.from(data, 'utf8') : Buffer.from(data);
    validation.payloadSize(bytes.length, validation.MAX_GUEST_RESULT_BYTES);
    if (this.transport === TRANSPORT_ZBRT) {
      const payload = await this.#withExecConn(
        (conn) => conn.fs(5, path, Buffer.from(JSON.stringify({ data: [...bytes], append: options.append ?? false, mode: null }), 'utf8')));
      const last = JSON.parse(payload.toString('utf8')) as Record<string, unknown>;
      if (typeof last.bytes_written !== 'number') {
        throw new DecodeError('guest response is missing bytes_written');
      }
      return last.bytes_written;
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

  /**
   * Open an interactive stream (§5). Fail closed before any frame: empty or
   * non-string argv is rejected on both transports; over ZBRT `pty=true` or a
   * non-empty `env` (and argc > 255) raise {@link ValidationError} before the
   * TCP connect.
   *
   * @param args Command + arguments (non-empty string array).
   * @param options Optional: `cwd` (guest default when null), `pty`
   *   (NDJSON only), `env` (NDJSON only).
   * @returns A {@link GuestStream} yielding started/stdout/stderr/exit events.
   * @throws {ValidationError} Local pre-send validation failed.
   * @throws {TransportError} Connection failure or read stall.
   * @throws {RemoteError} Guest error line / Error frame / handshake failure.
   * @throws {DecodeError} Malformed response.
   */
  async stream(args: readonly string[], options: StreamOptions = {}): Promise<GuestStream> {
    validation.argv(args);
    const cwd = options.cwd ?? null;
    if (cwd !== null) validation.filePath(cwd);
    const pty = options.pty ?? null;
    const env = options.env ?? null;
    if (this.transport === TRANSPORT_ZBRT) {
      // Fail closed: ZBRT v1 has neither a pty nor an env channel.
      if (pty === true) {
        throw new ValidationError('pty is not supported over the ZBRT transport');
      }
      if (env !== null && Object.keys(env).length > 0) {
        throw new ValidationError('env is not supported over the ZBRT transport');
      }
      // Fail closed before any connection: ZBRT encodes argc in one byte.
      validation.zbrtArgs(args);
      const conn = await this.#zbrt();
      const session = await conn.openStreamSession(args, cwd, Buffer.alloc(0), 0);
      return new ZbrtGuestStream(conn, session);
    }
    // PROTOCOL.md §2.2: optional keys are sent only when requested; the
    // default cwd is the guest's own (UNIFIED_API.md §5).
    const action: Record<string, unknown> = { action: 'stream', args: [...args] };
    if (cwd !== null) action.cwd = cwd;
    if (pty !== null) action.pty = pty;
    if (env !== null) action.env = env;
    return new NdjsonGuestStream(this.guestAddr, action, this.#guestTimeoutMs);
  }

  /**
   * Delete this sandbox via the controller (§4: 2xx and 404 are both success).
   *
   * @throws {ValidationError} Malformed sandbox id.
   * @throws {TransportError} Connection failure or timeout.
   * @throws {HttpStatusError} Non-2xx, non-404 controller answer.
   */
  async delete(): Promise<void> {
    this.#ndjsonPool?.dispose();
    for (const { conn } of this.#zbrtExecPool.splice(0)) conn.close();
    await this.#client.deleteSandbox(this.id);
  }

  async #zbrt(): Promise<ZbrtConnection> {
    const addr = parseAddress(this.guestAddr);
    const conn = addr.socketPath !== undefined
      ? new ZbrtConnection(this.guestAddr, this.#guestTimeoutMs)
      : new ZbrtConnection(addr.host!, addr.port!, this.#guestTimeoutMs);
    await conn.ready();
    return conn;
  }

  /** 借一条 exec 温连接：池里同步 pop + re-Hello 验活（死的丢弃继续找/
   * 回退新连接）。单线程 event loop 下同步 pop 无交错风险。 */
  async #borrowExecConn(): Promise<ZbrtConnection> {
    while (this.#zbrtExecPool.length > 0) {
      const { conn, lastUsed } = this.#zbrtExecPool.pop()!;
      // 热路径（背靠背操作）零额外 RTT：只对空闲 >1s 的连接验活。
      if (performance.now() - lastUsed < 1000) {
        return conn;
      }
      try {
        await conn.hello('rfb-sdk-node');
        return conn;
      } catch {
        conn.close();
      }
    }
    return this.#zbrt();
  }

  /** exec turn：借 → 用 → 干净结束归还（复用）→ 故障关闭。 */
  async #withExecConn<T>(op: (conn: ZbrtConnection) => Promise<T>): Promise<T> {
    const conn = await this.#borrowExecConn();
    try {
      const out = await op(conn);
      this.#zbrtExecPool.push({ conn, lastUsed: performance.now() });
      return out;
    } catch (error) {
      conn.close();
      // 写失败 = 请求未送达：换新连接重试一次；读超时/解码/guest 错误绝不重试。
      if (error instanceof TransportError && error.message.startsWith('zbrt write failed')) {
        const fresh = await this.#zbrt();
        try {
          const out = await op(fresh);
          this.#zbrtExecPool.push({ conn: fresh, lastUsed: performance.now() });
          return out;
        } catch (e2) {
          fresh.close();
          throw e2;
        }
      }
      throw error;
    }
  }

  async #guestRequest(action: Record<string, unknown>, timeoutMs: number = this.#guestTimeoutMs): Promise<NdjsonExchange> {
    const addr = parseAddress(this.guestAddr);
    if (this.#ndjsonPool === null) {
      this.#ndjsonPool = new NdjsonPool(addr.host!, addr.port!);
    }
    return this.#ndjsonPool.request(action, timeoutMs);
  }

  async #fsRequest(op: number, path: string, args: Record<string, unknown>): Promise<NdjsonExchange> {
    const action = { action: FS_ACTIONS[op], path, ...args };
    return this.#guestRequest(action);
  }

  /** ZBRT FsResult JSON — identical payload shape to the NDJSON tools (§2.4). */
  #fsResultJson(payload: Buffer): Record<string, unknown> {
    return JSON.parse(payload.toString('utf8')) as Record<string, unknown>;
  }

  #parseLs(last: Record<string, unknown>): DirEntry[] {
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

  #parseFind(last: Record<string, unknown>): string[] {
    this.#checkResultSize(last);
    const matches = last.matches;
    if (!Array.isArray(matches)) {
      throw new DecodeError('guest response is missing matches');
    }
    // Mirror the Python facade: find matches must be strings — coercing a
    // non-string entry would hide a decode divergence.
    if (!matches.every((match) => typeof match === 'string')) {
      throw new DecodeError('find matches must be strings');
    }
    return matches.map((match: string) => match);
  }

  #parseGrep(last: Record<string, unknown>): GrepMatch[] {
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
      // §6: UTF-8 replace decoding only — no trimming (mirrors Python's
      // stdout_text / the Rust baseline).
      stdoutText: stdout.toString('utf8'),
      stderrText: stderr.toString('utf8'),
      timedOut,
    };
  }
}

// The fs op codes are 1-based (ZBRT FS_OP_*: 1=ls 2=find 3=grep 4=read
// 5=write) — slot 0 stays unused or every NDJSON fs request lands on the
// WRONG action (ls silently became find, whose missing pattern is
// rejected by the agent as 'pattern must be a string').
const FS_ACTIONS: readonly (string | null)[] = [null, 'ls', 'find', 'grep', 'read', 'write'];

/** NDJSON interactive stream: started → stdout/stderr → exit (PROTOCOL.md §2.5). */
class NdjsonGuestStream implements GuestStream {
  #socket: net.Socket | null = null;
  #buffer = Buffer.alloc(0);
  #pending: StreamEvent[] = [];
  // Waiter queue (not a single slot): concurrent nextEvent callers must all
  // be woken or all but one hang forever.
  #waiters: (() => void)[] = [];
  #closed = false;
  #terminal = false;
  #stopped = false;
  #failure: RfbError | null = null;
  // Agent auth (FORKD_AGENT_TOKEN): the auth line goes out first; the real
  // stream action and any early in/stop lines wait for the auth ack so they
  // can never precede it on the wire.
  #awaitingAuth = false;
  #deferred: Buffer[] = [];
  readonly #address: string;
  readonly #action: Record<string, unknown>;
  readonly #timeoutMs: number;

  constructor(address: string, action: Record<string, unknown>, timeoutMs: number) {
    this.#address = address;
    this.#action = action;
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
    if (this.#terminal || this.#stopped || this.#closed) {
      throw new RemoteError('guest stream is no longer running');
    }
    if (this.#socket === null) await this.#connect();
    this.#writeLine(Buffer.from(JSON.stringify({ in: text }) + '\n', 'utf8'));
  }

  /** Idempotent stop: terminates the child; the exit frame follows. */
  async stop(): Promise<void> {
    if (this.#stopped || this.#terminal || this.#closed) return;
    this.#stopped = true;
    if (this.#socket === null) await this.#connect();
    this.#writeLine(Buffer.from(JSON.stringify({ action: 'stop' }) + '\n', 'utf8'));
  }

  /**
   * Write one client line. While an agent auth round-trip is pending the line
   * is queued and flushed right after the ack (mirrors Python `_guest.py`,
   * which completes the auth exchange before the caller can write).
   */
  #writeLine(data: Buffer): void {
    if (this.#awaitingAuth) {
      this.#deferred.push(data);
      return;
    }
    this.#socket?.write(data);
  }

  #flushDeferred(): void {
    const queued = this.#deferred.splice(0);
    for (const data of queued) this.#socket?.write(data);
  }

  #actionLine(): Buffer {
    return Buffer.from(JSON.stringify(this.#action) + '\n', 'utf8');
  }

  async #connect(): Promise<void> {
    const addr = parseAddress(this.#address);
    const host = addr.host ?? '';
    const port = addr.port ?? 0;
    const socket = net.createConnection({ host, port });
    this.#socket = socket;
    const token = agentAuthToken();
    this.#awaitingAuth = token !== null;
    socket.setTimeout(this.#timeoutMs);
    socket.setNoDelay(true);
    socket.on('error', (error) => {
      this.#fail(new TransportError(`stream error: ${error.message}`));
    });
    socket.on('timeout', () => {
      this.#fail(new TransportError('stream timed out'));
    });
    socket.on('data', (chunk: Buffer) => {
      this.#buffer = Buffer.concat([this.#buffer, chunk]);
      this.#pump();
    });
    socket.on('close', () => {
      this.#closed = true;
      this.#pump();
      if (!this.#terminal && this.#failure === null && this.#buffer.length > 0) {
        // The Rust baseline rejects a stream that ends mid-line.
        this.#fail(new DecodeError('guest stream ended with an unterminated line'));
      }
    });
    if (token === null) {
      socket.write(this.#actionLine());
    } else {
      // The auth line is always the first line on the connection.
      socket.write(Buffer.from(JSON.stringify({ action: 'auth', token }) + '\n', 'utf8'));
    }
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
      if (typeof value.error === 'string') {
        this.#fail(new RemoteError(value.error));
        return;
      }
      if (this.#awaitingAuth) {
        // Intercept before the event mapping: the ack only unblocks the real
        // stream action; anything else means the agent refused the connection.
        if (value.action === 'auth' && value.ok === true) {
          this.#awaitingAuth = false;
          this.#socket?.write(this.#actionLine());
          this.#flushDeferred();
          continue;
        }
        this.#fail(new RemoteError('agent auth failed'));
        return;
      }
      // PROTOCOL.md §2.5 order: started → exit_code → done → output keys.
      if (value.started === true || value.stream === 'started' || value.event === 'started') {
        this.#pending.push({ kind: 'started', data: Buffer.alloc(0), code: null });
      } else if ('exit_code' in value) {
        this.#terminal = true;
        const code = value.exit_code;
        this.#pending.push({
          kind: 'exit',
          data: Buffer.alloc(0),
          code: typeof code === 'number' && Number.isInteger(code) ? code : null,
        });
      } else if (value.done === true) {
        this.#terminal = true;
        this.#pending.push({ kind: 'exit', data: Buffer.alloc(0), code: null });
      } else {
        try {
          const stdout = streamBytes(value, 'stdout', 'out');
          const stderr = streamBytes(value, 'stderr', 'err');
          if (stdout !== null) {
            this.#pending.push({ kind: 'stdout', data: stdout, code: null });
          } else if (stderr !== null) {
            this.#pending.push({ kind: 'stderr', data: stderr, code: null });
          }
          // Unrecognized event keys are ignored so future frame additions do
          // not break existing clients.
        } catch (error) {
          this.#fail(error as RfbError);
          return;
        }
      }
      if (this.#terminal) this.#socket?.destroy();
      this.#wake();
    }
    if (this.#buffer.length > validation.MAX_LINE_BYTES) {
      this.#fail(new DecodeError(`guest stream line exceeded ${validation.MAX_LINE_BYTES} bytes`));
      return;
    }
    this.#wake();
  }

  #fail(error: RfbError): void {
    if (this.#failure === null) {
      this.#failure = error;
      this.#socket?.destroy();
    }
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
  #session: {
    nextEvent(): Promise<{ stream: number; data: Buffer; code: number | null; started?: true } | null>;
    stop(): Promise<void>;
    terminal: boolean;
    stopped: boolean;
  };

  constructor(
    conn: ZbrtConnection,
    session: {
      nextEvent(): Promise<{ stream: number; data: Buffer; code: number | null; started?: true } | null>;
      stop(): Promise<void>;
      terminal: boolean;
      stopped: boolean;
    },
  ) {
    this.#conn = conn;
    this.#session = session;
  }

  async nextEvent(): Promise<StreamEvent | null> {
    let event;
    try {
      event = await this.#session.nextEvent();
    } catch (error) {
      // Error/Decode/Transport = 终态：连接一并释放，错误原样上抛。
      this.#conn.close();
      throw error;
    }
    if (event === null) {
      // Session over: release the transport socket instead of leaking it
      // until GC/process exit.
      this.#conn.close();
      return null;
    }
    if (event.started === true) {
      // ZBRT has no started frame: the session synthesizes one as the first
      // event of every turn (cross-language contract).
      return { kind: 'started', data: Buffer.alloc(0), code: null };
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

  /** ZBRT v1 carries stdin only inside the Execute payload (UNIFIED_API.md §5). */
  async sendInput(_text: string): Promise<void> {
    if (this.#session.terminal || this.#session.stopped) {
      throw new RemoteError('guest stream is no longer running');
    }
    throw new RemoteError('stdin is not supported over the zbrt transport');
  }

  /** Idempotent stop: Cancel targeting this turn, expecting CancelAck. */
  async stop(): Promise<void> {
    await this.#session.stop();
  }
}
