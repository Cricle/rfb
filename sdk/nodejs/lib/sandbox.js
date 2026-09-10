/**
 * One connected sandbox. Guest operations run over the NDJSON or the ZBRT
 * transport — same names, same result shapes (UNIFIED_API.md §4–§5).
 */
import { RemoteError, TransportError, ValidationError } from './errors.js';

export const TRANSPORT_NDJSON = 'ndjson';
export const TRANSPORT_ZBRT = 'zbrt';
import { parseAddress, request as ndjsonRequest } from './ndjson.js';
import * as validation from './validation.js';
import { ZbrtConnection } from './zbrt-connection.js';

/** NDJSON status code extraction: exit_code first, then status, else fallback. */
function statusCode(value, fallback) {
  if (typeof value.exit_code === 'number') return value.exit_code;
  if (typeof value.status === 'number') return value.status;
  return fallback;
}

/** current-key-first lookup with the legacy agent key as fallback. */
function firstOf(value, current, legacy) {
  return value?.[current] ?? value?.[legacy];
}

function valueBytes(value) {
  if (value === undefined || value === null) return Buffer.alloc(0);
  if (typeof value === 'string') return Buffer.from(value, 'utf8');
  if (Array.isArray(value)) return Buffer.from(value);
  throw new DecodeError('expected a byte array or string');
}

export class Sandbox {
  constructor(info, client, transport) {
    this.info = info;
    this.id = info.id;
    this.snapshotTag = info.snapshot_tag ?? '';
    this.guestAddr = info.guest_addr ?? '';
    this.transport = transport;
    this.#client = client;
  }

  #client;

  /** Guest health check: true only when pong=true. */
  async ping() {
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

  /**
   * Execute one command. cwd defaults to /workspace over NDJSON; the NDJSON
   * wire has no stdin channel, so non-empty stdin is silently dropped there
   * (Rust baseline) and delivered over ZBRT. stdout/stderr accept both the
   * current and the legacy agent keys.
   */
  async exec(args, { cwd = '/workspace', timeoutS = 60, stdin = null } = {}) {
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
        return {
          exitCode: exec.code,
          stdout: exec.stdout,
          stderr: exec.stderr,
          stdoutText: exec.stdout.toString('utf8'),
          stderrText: exec.stderr.toString('utf8'),
          timedOut: exec.timedOut,
        };
      } finally {
        conn.close();
      }
    }
    const { last } = await this.#guestRequest({
      action: 'exec',
      cwd,
      timeout: Math.ceil(timeoutS),
      args,
    });
    return {
      exitCode: statusCode(last, -1),
      stdout: valueBytes(firstOf(last, 'stdout', 'out')),
      stderr: valueBytes(firstOf(last, 'stderr', 'err')),
      stdoutText: valueBytes(firstOf(last, 'stdout', 'out')).toString('utf8').trimEnd(),
      stderrText: valueBytes(firstOf(last, 'stderr', 'err')).toString('utf8'),
      timedOut: last.timed_out === true,
    };
  }

  /** Evaluate a code snippet; output maps to stdout. */
  async eval(code, { cwd, timeoutS } = {}) {
    validation.evalCode(code);
    if (cwd !== undefined && cwd !== null) validation.filePath(cwd);
    if (timeoutS !== undefined && timeoutS !== null) validation.timeoutS(timeoutS);
    if (this.transport === TRANSPORT_ZBRT) {
      const conn = await this.#zbrt();
      try {
        const exec = await conn.execute(
          ['eval', code],
          cwd ?? undefined,
          Buffer.alloc(0),
          timeoutS === undefined || timeoutS === null ? 0 : Math.ceil(timeoutS * 1000),
        );
        return {
          exitCode: exec.code,
          stdout: exec.stdout,
          stderr: Buffer.alloc(0),
          stdoutText: exec.stdout.toString('utf8'),
          stderrText: '',
          timedOut: exec.timedOut,
        };
      } finally {
        conn.close();
      }
    }
    const action = { action: 'eval', code };
    if (cwd !== undefined && cwd !== null) action.cwd = cwd;
    if (timeoutS !== undefined && timeoutS !== null) action.timeout = Math.ceil(timeoutS);
    const { last } = await this.#guestRequest(action);
    const out = valueBytes(firstOf(last, 'out', 'output'));
    return {
      exitCode: statusCode(last, 0),
      stdout: out,
      stderr: Buffer.alloc(0),
      stdoutText: out.toString('utf8'),
      stderrText: '',
      timedOut: last.timed_out === true,
    };
  }

  /** List directory entries (default path "."). */
  async ls(path = '.') {
    validation.fsPath(path);
    const { last } = await this.#fsRequest(1, path, {
      max_results: validation.MAX_GUEST_RESULTS,
    });
    this.#checkResultSize(last);
    return (last.entries ?? []).map((entry) => ({
      name: entry.name ?? '',
      isDir: entry.is_dir === true,
      size: entry.size ?? null,
    }));
  }

  /** Find workspace paths matching a glob-ish pattern. */
  async find(path, pattern) {
    if (pattern === undefined) {
      pattern = path;
      path = '.';
    }
    validation.fsPath(path);
    validation.pattern(pattern);
    const { last } = await this.#fsRequest(2, path, {
      max_results: validation.MAX_GUEST_RESULTS,
      pattern,
    });
    this.#checkResultSize(last);
    return last.matches ?? [];
  }

  /** Grep file contents; matches carry path/line/column/text. */
  async grep(path, pattern) {
    if (pattern === undefined) {
      pattern = path;
      path = '.';
    }
    validation.fsPath(path);
    validation.pattern(pattern);
    const { last } = await this.#fsRequest(3, path, {
      max_results: validation.MAX_GUEST_RESULTS,
      pattern,
      max_bytes: validation.MAX_GUEST_RESULT_BYTES,
    });
    this.#checkResultSize(last);
    return (last.matches ?? []).map((match) => ({
      path: match.path ?? '',
      line: match.line ?? null,
      column: match.column ?? null,
      text: match.text ?? '',
    }));
  }

  /** Read a guest file: `{data, truncated, totalBytes}`. */
  async read(path, { offset, maxBytes } = {}) {
    validation.filePath(path);
    if (maxBytes !== undefined && maxBytes !== null) {
      validation.limit(maxBytes, validation.MAX_GUEST_RESULT_BYTES);
    }
    if (this.transport === TRANSPORT_ZBRT) {
      const conn = await this.#zbrt();
      try {
        // PROTOCOL.md §3.3: the Fs data object always carries the keys, with
        // null for absent optionals.
        const payload = await conn.fs(4, path, { offset, max_bytes: maxBytes });
        return fileReadFromJson(payload);
      } finally {
        conn.close();
      }
    }
    const action = { action: 'read', path };
    if (offset !== undefined && offset !== null) action.offset = offset;
    if (maxBytes !== undefined && maxBytes !== null) action.max_bytes = maxBytes;
    const { last } = await this.#guestRequest(action);
    const data = valueBytes(last.data);
    return {
      data,
      truncated: last.truncated === true,
      totalBytes: last.total_bytes ?? null,
    };
  }

  /** Write (or append to) a guest file; returns bytes_written. */
  async write(path, data, { append = false } = {}) {
    validation.filePath(path);
    const bytes = typeof data === 'string' ? Buffer.from(data, 'utf8') : Buffer.from(data ?? []);
    validation.payloadSize(bytes.length, validation.MAX_GUEST_RESULT_BYTES);
    if (this.transport === TRANSPORT_ZBRT) {
      const conn = await this.#zbrt();
      try {
        // PROTOCOL.md §3.3: {"data":[..],"append":..,"mode":null}
        const payload = await conn.fs(5, path, {
          data: [...bytes],
          append,
          mode: null,
        });
        const last = JSON.parse(payload.toString('utf8'));
        return typeof last.bytes_written === 'number' ? last.bytes_written : 0;
      } finally {
        conn.close();
      }
    }
    const { last } = await this.#guestRequest({
      action: 'write',
      path,
      data: [...bytes],
      append,
    });
    return typeof last.bytes_written === 'number' ? last.bytes_written : 0;
  }

  /**
   * Interactive stream: started → stdout/stderr events → exit; send_input is
   * NDJSON-only; stop is idempotent. See UNIFIED_API.md §5.
   */
  async stream(args, { cwd = '/workspace', pty = null, env = null } = {}) {
    validation.argv(args);
    validation.filePath(cwd);
    if (this.transport === TRANSPORT_ZBRT) {
      // Fail closed: ZBRT v1 has neither a pty nor an env channel.
      if (pty === true) {
        throw new ValidationError('pty is not supported over the ZBRT transport');
      }
      if (env !== null && env !== undefined && Object.keys(env).length > 0) {
        throw new ValidationError('env is not supported over the ZBRT transport');
      }
      const conn = await this.#zbrt();
      const session = await conn.openStreamSession(args, cwd, Buffer.alloc(0), 0);
      return new ZbrtGuestStream(conn, session);
    }
    return new NdjsonGuestStream(this.guestAddr, args, cwd, this.#client.timeoutS * 1000);
  }

  /** Delete the sandbox via the controller (2xx and 404 are both success). */
  async delete() {
    await this.#client.deleteSandbox(this.id);
  }

  async #zbrt() {
    const conn = new ZbrtConnection(
      ...(() => {
        const { host, port } = parseAddress(this.guestAddr);
        return [host, port];
      })(),
      10_000,
    );
    await conn.ready();
    return conn;
  }

  async #guestRequest(action) {
    const { host, port } = parseAddress(this.guestAddr);
    return ndjsonRequest(host, port, this.#client.timeoutS * 1000, action);
  }

  /** Structured fs RPC (opcodes 1=ls 2=find 3=grep 4=read 5=write) over NDJSON. */
  async #fsRequest(op, path, args) {
    const action = { action: ['ls', 'find', 'grep', 'read', 'write'][op - 1], path, ...args };
    return this.#guestRequest(action);
  }

  #checkResultSize(last) {
    if (Buffer.byteLength(JSON.stringify(last), 'utf8') > validation.MAX_GUEST_RESULT_BYTES) {
      throw new TransportError('guest response exceeded limit');
    }
    const results = last.results;
    if (Array.isArray(results) && results.length > validation.MAX_GUEST_RESULTS) {
      throw new RemoteError('guest result limit exceeded');
    }
  }
}

/** NDJSON interactive stream: started → out/err → exit, with send_input/stop. */
class NdjsonGuestStream {
  constructor(address, args, cwd, timeoutMs) {
    this.#address = address;
    this.#args = args;
    this.#cwd = cwd;
    this.#timeoutMs = timeoutMs;
  }

  #address;
  #args;
  #cwd;
  #timeoutMs;
  #socket = null;
  #buffer = Buffer.alloc(0);
  #pending = [];
  #notify = null;
  #started = false;
  #closed = false;
  #failure = null;

  #fail(error) {
    if (this.#failure === null) this.#failure = error;
    this.#wake();
  }

  #wake() {
    const n = this.#notify;
    this.#notify = null;
    n?.();
  }

  async #connect() {
    const { host, port } = parseAddress(this.#address);
    return new Promise((resolve, reject) => {
      const socket = net.createConnection({ host, port });
      socket.setTimeout(this.#timeoutMs);
      socket.on('error', (error) => this.#fail(new TransportError(`stream error: ${error.message}`)));
      socket.on('timeout', () => this.#fail(new TransportError('stream timed out')));
      socket.on('data', (chunk) => {
        this.#buffer = Buffer.concat([this.#buffer, chunk]);
        this.#pump();
      });
      socket.on('close', () => {
        this.#closed = true;
        this.#pump();
      });
      socket.on('connect', () => {
        socket.setNoDelay(true);
        resolve(socket);
      });
    });
  }

  #pump() {
    while (true) {
      const nl = this.#buffer.indexOf('\n');
      if (nl < 0) break;
      let raw = this.#buffer.subarray(0, nl);
      this.#buffer = this.#buffer.subarray(nl + 1);
      while (raw.length > 0 && (raw[raw.length - 1] === 0x0a || raw[raw.length - 1] === 0x0d)) {
        raw = raw.subarray(0, raw.length - 1);
      }
      if (raw.length === 0) continue;
      let value;
      try {
        value = JSON.parse(raw.toString('utf8'));
      } catch (error) {
        this.#fail(new DecodeError(`invalid stream JSON: ${error.message}`));
        return;
      }
      if (value && typeof value.error === 'string') {
        this.#fail(new RemoteError(value.error));
        return;
      }
      if (value.stream === 'started' || value.started === true) {
        this.#pending.push({ kind: 'started', data: Buffer.alloc(0), code: null });
      } else if (value.exit_code !== undefined) {
        this.#pending.push({
          kind: 'exit',
          data: Buffer.alloc(0),
          code: typeof value.exit_code === 'number' ? value.exit_code : null,
        });
      } else if (value.done === true) {
        this.#pending.push({ kind: 'exit', data: Buffer.alloc(0), code: null });
      } else if (typeof value.out === 'string' || Array.isArray(value.out)) {
        this.#pending.push({ kind: 'stdout', data: Buffer.from(value.out), code: null });
      } else if (typeof value.err === 'string' || Array.isArray(value.err)) {
        this.#pending.push({ kind: 'stderr', data: Buffer.from(value.err), code: null });
      }
      this.#wake();
    }
  }

  async #ensureConnected() {
    if (this.#socket === null) {
      this.#socket = await this.#connect();
      this.#socket.write(
        Buffer.from(
          JSON.stringify({ action: 'stream', args: this.#args, cwd: this.#cwd }) + '\n',
          'utf8',
        ),
      );
    }
    if (this.#failure !== null) throw this.#failure;
  }

  /** Next event: {kind, data, code} — null after a clean close. */
  async nextEvent() {
    while (true) {
      await this.#ensureConnected();
      if (this.#pending.length > 0) return this.#pending.shift();
      if (this.#failure !== null) throw this.#failure;
      if (this.#closed) return null;
      await new Promise((resolve) => {
        this.#notify = resolve;
      });
    }
  }

  /** Send one stdin payload to the child (NDJSON only). */
  async sendInput(text) {
    await this.#ensureConnected();
    this.#socket.write(Buffer.from(JSON.stringify({ in: text }) + '\n', 'utf8'));
  }

  /** Idempotent stop: terminates the child and closes the stream. */
  async stop() {
    await this.#ensureConnected();
    this.#socket.write(Buffer.from(JSON.stringify({ action: 'stop' }) + '\n', 'utf8'));
  }
}

/** ZBRT interactive stream: Output frames → stdout/stderr, Exit → exit. */
class ZbrtGuestStream {
  constructor(conn, session) {
    this.#conn = conn;
    this.#session = session;
  }

  #conn;
  #session;

  /** Next event: {kind, data, code} — null after the terminal Exit. */
  async nextEvent() {
    const event = await this.#session.nextEvent();
    if (event === null) return null;
    if (event.code !== null) {
      return { kind: 'exit', data: Buffer.alloc(0), code: event.code };
    }
    return {
      kind: event.stream === 0 ? 'stdout' : 'stderr',
      data: event.data,
      code: null,
    };
  }

  /** ZBRT v1 has no stdin channel — sends raise TransportError. */
  async sendInput() {
    throw new TransportError('zbrt v1 has no input channel');
  }

  /** Idempotent stop: Cancel targeting this turn, expecting CancelAck. */
  async stop() {
    await this.#session.stop();
  }
}

function fileReadFromJson(payload) {
  const node = JSON.parse(payload.toString('utf8'));
  const data = Array.isArray(node.data) ? Buffer.from(node.data) : Buffer.alloc(0);
  return {
    data,
    truncated: node.truncated === true,
    totalBytes: typeof node.total_bytes === 'number' ? node.total_bytes : null,
  };
}
