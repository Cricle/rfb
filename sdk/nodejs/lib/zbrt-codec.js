/**
 * Strict ZBRT v1 payload codecs (PROTOCOL.md §3.2), mirroring
 * `sdk/java/src/main/java/io/rfb/sdk/internal/ZbrtCodec.java`:
 * u32 BE length-prefixed UTF-8 text, byte arrays, optional-flag forms.
 * INTERNAL.
 */
import { DecodeError } from './errors.js';

export const MAX_CAPABILITIES = 255;
export const MAX_ARGV = 255;

class Reader {
  constructor(buffer) {
    this.buffer = buffer;
    this.pos = 0;
  }

  hasRemaining() {
    return this.pos < this.buffer.length;
  }

  #need(n) {
    if (this.buffer.length - this.pos < n) {
      throw new DecodeError('truncated payload');
    }
  }

  u8() {
    this.#need(1);
    return this.buffer[this.pos++];
  }

  flag() {
    return this.u8() !== 0;
  }

  u32() {
    this.#need(4);
    const v =
      ((this.buffer[this.pos] & 0xff) << 24) |
      ((this.buffer[this.pos + 1] & 0xff) << 16) |
      ((this.buffer[this.pos + 2] & 0xff) << 8) |
      (this.buffer[this.pos + 3] & 0xff);
    this.pos += 4;
    return v >>> 0;
  }

  i32() {
    return this.u32() | 0;
  }

  bytes(n) {
    this.#need(n);
    const out = this.buffer.subarray(this.pos, this.pos + n);
    this.pos += n;
    return out;
  }

  prefixedBytes() {
    const n = this.u32();
    return this.bytes(n);
  }

  text() {
    return this.prefixedBytes().toString('utf8');
  }
}

function putU32(chunks, v) {
  chunks.push(Buffer.from([(v >>> 24) & 0xff, (v >>> 16) & 0xff, (v >>> 8) & 0xff, v & 0xff]));
}

function putText(chunks, text) {
  const b = Buffer.from(text, 'utf8');
  putU32(chunks, b.length);
  chunks.push(b);
}

function putPrefixed(chunks, bytes) {
  putU32(chunks, bytes.length);
  chunks.push(bytes);
}

function putFlagText(chunks, text) {
  if (text !== null && text !== undefined) {
    chunks.push(Buffer.from([1]));
    putText(chunks, text);
  } else {
    chunks.push(Buffer.from([0]));
  }
}

function checkTrailing(reader) {
  if (reader.hasRemaining()) {
    throw new DecodeError('trailing payload');
  }
}

function checkCount(count) {
  if (count > MAX_CAPABILITIES) {
    throw new DecodeError('too many capabilities');
  }
}

function concat(chunks) {
  return Buffer.concat(chunks);
}

// ---- Hello / HelloAck ------------------------------------------------

export function encodeHello(client, capabilities) {
  return encodeHelloLike(client, capabilities);
}

export function encodeHelloAck(server, capabilities) {
  return encodeHelloLike(server, capabilities);
}

function encodeHelloLike(name, capabilities) {
  checkCount(capabilities.length);
  const chunks = [Buffer.from([0])];
  putText(chunks, name);
  chunks.push(Buffer.from([capabilities.length]));
  for (const cap of capabilities) {
    putText(chunks, cap);
  }
  return concat(chunks);
}

export function decodeHelloAck(payload) {
  const r = new Reader(payload);
  const server = r.text();
  const n = r.u8();
  const capabilities = [];
  for (let i = 0; i < n; i++) {
    capabilities.push(r.text());
  }
  checkTrailing(r);
  return { server, capabilities };
}

// ---- Execute -----------------------------------------------------------

export function encodeExecute(argv, cwd, stdin, timeoutMs) {
  if (argv.length > MAX_ARGV) {
    throw new DecodeError('too many arguments');
  }
  if (timeoutMs < 0 || timeoutMs > 0xffffffff) {
    throw new DecodeError('timeout_ms out of u32 range');
  }
  const chunks = [Buffer.from([argv.length])];
  for (const arg of argv) {
    putText(chunks, arg);
  }
  putFlagText(chunks, cwd);
  putPrefixed(chunks, stdin);
  putU32(chunks, timeoutMs);
  return concat(chunks);
}

export function decodeExecute(payload) {
  const r = new Reader(payload);
  const argc = r.u8();
  const argv = [];
  for (let i = 0; i < argc; i++) {
    argv.push(r.text());
  }
  const cwd = r.flag() ? r.text() : null;
  const stdin = r.prefixedBytes();
  const timeoutMs = r.u32();
  checkTrailing(r);
  return { argv, cwd, stdin, timeoutMs };
}

// ---- Output ------------------------------------------------------------

export function encodeOutput(stream, data) {
  return concat([Buffer.from([stream]), (() => {
    const chunks = [];
    putPrefixed(chunks, data);
    return concat(chunks);
  })()]);
}

export function decodeOutput(payload) {
  const r = new Reader(payload);
  const stream = r.u8();
  const data = r.prefixedBytes();
  checkTrailing(r);
  return { stream, data };
}

// ---- Exit ---------------------------------------------------------------

export function encodeExit(code, signal) {
  const chunks = [];
  putU32(chunks, code);
  if (signal !== null && signal !== undefined) {
    chunks.push(Buffer.from([1]));
    putU32(chunks, signal);
  } else {
    chunks.push(Buffer.from([0]));
  }
  return concat(chunks);
}

export function decodeExit(payload) {
  const r = new Reader(payload);
  const code = r.i32();
  let signal = null;
  if (r.flag()) {
    signal = r.u32();
  }
  checkTrailing(r);
  return { code, signal };
}

// ---- Cancel --------------------------------------------------------------

export function encodeCancel(reason, target16) {
  const chunks = [];
  putFlagText(chunks, reason);
  if (target16 !== null && target16 !== undefined) {
    if (target16.length !== 16) {
      throw new DecodeError('cancel target must be 16 bytes');
    }
    chunks.push(Buffer.from([1]));
    chunks.push(target16);
  } else {
    chunks.push(Buffer.from([0]));
  }
  return concat(chunks);
}

export function decodeCancel(payload) {
  const r = new Reader(payload);
  const reason = r.flag() ? r.text() : null;
  let target = null;
  if (!r.hasRemaining()) {
    target = null; // legacy payload without the target byte
  } else if (r.flag()) {
    target = r.bytes(16);
  } else {
    target = null;
  }
  checkTrailing(r);
  return { reason, target };
}

// ---- Fs --------------------------------------------------------------------

export function encodeFs(op, path, data) {
  const chunks = [Buffer.from([op])];
  putText(chunks, path);
  putPrefixed(chunks, data);
  return concat(chunks);
}

export function decodeFs(payload) {
  const r = new Reader(payload);
  const op = r.u8();
  const path = r.text();
  const data = r.prefixedBytes();
  checkTrailing(r);
  return { op, path, data };
}

// ---- Health / HealthAck ------------------------------------------------------

export function encodeHealth(healthy, message) {
  const chunks = [Buffer.from([healthy ? 1 : 0])];
  putFlagText(chunks, message);
  return concat(chunks);
}

export function decodeHealth(payload) {
  const r = new Reader(payload);
  const healthy = r.flag();
  const message = r.flag() ? r.text() : null;
  checkTrailing(r);
  return { healthy, message };
}

// ---- Error -------------------------------------------------------------

export function encodeError(code, message) {
  const chunks = [];
  putU32(chunks, code);
  putText(chunks, message);
  return concat(chunks);
}

export function decodeError(payload) {
  const r = new Reader(payload);
  const code = r.u32();
  const message = r.text();
  checkTrailing(r);
  return { code, message };
}
