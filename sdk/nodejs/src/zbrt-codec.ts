/**
 * Strict ZBRT v1 payload codecs (PROTOCOL.md §3.2): u32 BE length-prefixed
 * UTF-8 text, byte arrays, optional-flag forms. Mirrors the Java ZbrtCodec.
 * INTERNAL.
 */
import { DecodeError } from './errors.js';

export const MAX_CAPABILITIES = 255;
export const MAX_ARGV = 255;
export const MAX_U32 = 0xffffffff;

class Reader {
  #buffer: Buffer;
  #pos = 0;

  constructor(buffer: Buffer) {
    this.#buffer = buffer;
  }

  hasRemaining(): boolean {
    return this.#pos < this.#buffer.length;
  }

  #need(n: number): void {
    if (this.#buffer.length - this.#pos < n) {
      throw new DecodeError('truncated payload');
    }
  }

  u8(): number {
    this.#need(1);
    // `#need` above guarantees at least one readable byte.
    return this.#buffer[this.#pos++]!;
  }

  flag(): boolean {
    return this.u8() !== 0;
  }

  u32(): number {
    this.#need(4);
    const v = this.#buffer.readUInt32BE(this.#pos);
    this.#pos += 4;
    return v;
  }

  i32(): number {
    return this.u32() | 0;
  }

  bytes(n: number): Buffer {
    this.#need(n);
    const out = this.#buffer.subarray(this.#pos, this.#pos + n);
    this.#pos += n;
    return out;
  }

  prefixedBytes(): Buffer {
    const n = this.u32();
    return this.bytes(n);
  }

  text(): string {
    return this.prefixedBytes().toString('utf8');
  }
}

function putU32(chunks: Buffer[], v: number): void {
  chunks.push(Buffer.from([(v >>> 24) & 0xff, (v >>> 16) & 0xff, (v >>> 8) & 0xff, v & 0xff]));
}

function putText(chunks: Buffer[], text: string): void {
  const b = Buffer.from(text, 'utf8');
  putU32(chunks, b.length);
  chunks.push(b);
}

function putPrefixed(chunks: Buffer[], bytes: Uint8Array): void {
  putU32(chunks, bytes.length);
  chunks.push(Buffer.from(bytes));
}

function putFlagText(chunks: Buffer[], text: string | null | undefined): void {
  if (text !== null && text !== undefined) {
    chunks.push(Buffer.from([1]));
    putText(chunks, text);
  } else {
    chunks.push(Buffer.from([0]));
  }
}

function checkTrailing(r: Reader): void {
  if (r.hasRemaining()) {
    throw new DecodeError('trailing payload');
  }
}

function checkCount(count: number): void {
  if (count > MAX_CAPABILITIES) {
    throw new DecodeError('too many capabilities');
  }
}

function concat(chunks: Buffer[]): Buffer {
  return Buffer.concat(chunks);
}

// ---- Hello / HelloAck ------------------------------------------------

export function encodeHello(client: string, capabilities: readonly string[]): Buffer {
  return encodeHelloLike(client, capabilities);
}

export function encodeHelloAck(server: string, capabilities: readonly string[]): Buffer {
  return encodeHelloLike(server, capabilities);
}

function encodeHelloLike(name: string, capabilities: readonly string[]): Buffer {
  checkCount(capabilities.length);
  const chunks: Buffer[] = [];
  putText(chunks, name);
  chunks.push(Buffer.from([capabilities.length]));
  for (const cap of capabilities) {
    putText(chunks, cap);
  }
  return concat(chunks);
}

export interface HelloAck {
  server: string;
  capabilities: string[];
}

export function decodeHelloAck(payload: Uint8Array): HelloAck {
  const r = new Reader(Buffer.from(payload));
  const server = r.text();
  const n = r.u8();
  const capabilities: string[] = [];
  for (let i = 0; i < n; i++) {
    capabilities.push(r.text());
  }
  checkTrailing(r);
  return { server, capabilities };
}

// ---- Execute -----------------------------------------------------------

export interface Execute {
  argv: string[];
  cwd: string | null;
  stdin: Buffer;
  timeoutMs: number;
}

export function encodeExecute(
  argv: readonly string[],
  cwd: string | null | undefined,
  stdin: Uint8Array,
  timeoutMs: number,
): Buffer {
  if (argv.length > MAX_ARGV) {
    throw new DecodeError('too many arguments');
  }
  if (timeoutMs < 0 || timeoutMs > MAX_U32) {
    throw new DecodeError('timeout_ms out of u32 range');
  }
  const chunks: Buffer[] = [Buffer.from([argv.length])];
  for (const arg of argv) {
    putText(chunks, arg);
  }
  putFlagText(chunks, cwd ?? null);
  putPrefixed(chunks, stdin);
  putU32(chunks, timeoutMs);
  return concat(chunks);
}

export function decodeExecute(payload: Uint8Array): Execute {
  const r = new Reader(Buffer.from(payload));
  const argc = r.u8();
  const argv: string[] = [];
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

export interface Output {
  stream: number;
  data: Buffer;
}

export function encodeOutput(stream: number, data: Uint8Array): Buffer {
  const chunks: Buffer[] = [Buffer.from([stream])];
  putPrefixed(chunks, data);
  return concat(chunks);
}

export function decodeOutput(payload: Uint8Array): Output {
  const r = new Reader(Buffer.from(payload));
  const stream = r.u8();
  const data = r.prefixedBytes();
  checkTrailing(r);
  return { stream, data };
}

// ---- Exit ---------------------------------------------------------------

export interface Exit {
  code: number;
  signal: number | null;
}

export function encodeExit(code: number, signal: number | null | undefined): Buffer {
  const chunks: Buffer[] = [];
  putU32(chunks, code);
  if (signal !== null && signal !== undefined) {
    chunks.push(Buffer.from([1]));
    putU32(chunks, signal);
  } else {
    chunks.push(Buffer.from([0]));
  }
  return concat(chunks);
}

export function decodeExit(payload: Uint8Array): Exit {
  const r = new Reader(Buffer.from(payload));
  const code = r.i32();
  const signal = r.flag() ? r.u32() : null;
  checkTrailing(r);
  return { code, signal };
}

// ---- Cancel --------------------------------------------------------------

export interface Cancel {
  reason: string | null;
  target: Buffer | null;
}

export function encodeCancel(reason: string | null | undefined, target16: Uint8Array | null): Buffer {
  const chunks: Buffer[] = [];
  putFlagText(chunks, reason);
  if (target16 !== null && target16 !== undefined) {
    if (target16.length !== 16) {
      throw new DecodeError('cancel target must be 16 bytes');
    }
    chunks.push(Buffer.from([1]));
    chunks.push(Buffer.from(target16));
  } else {
    chunks.push(Buffer.from([0]));
  }
  return concat(chunks);
}

export function decodeCancel(payload: Uint8Array): Cancel {
  const r = new Reader(Buffer.from(payload));
  const reason = r.flag() ? r.text() : null;
  let target: Buffer | null = null;
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

export interface Fs {
  op: number;
  path: string;
  data: Buffer;
}

export function encodeFs(op: number, path: string, data: Uint8Array): Buffer {
  const chunks: Buffer[] = [Buffer.from([op])];
  putText(chunks, path);
  putPrefixed(chunks, data);
  return concat(chunks);
}

export function decodeFs(payload: Uint8Array): Fs {
  const r = new Reader(Buffer.from(payload));
  const op = r.u8();
  const path = r.text();
  const data = r.prefixedBytes();
  checkTrailing(r);
  return { op, path, data };
}

// ---- Health / HealthAck ----------------------------------------------

export interface Health {
  healthy: boolean;
  message: string | null;
}

export function encodeHealth(healthy: boolean, message: string | null | undefined): Buffer {
  const chunks: Buffer[] = [Buffer.from([healthy ? 1 : 0])];
  putFlagText(chunks, message);
  return concat(chunks);
}

export function decodeHealth(payload: Uint8Array): Health {
  const r = new Reader(Buffer.from(payload));
  const healthy = r.flag();
  const message = r.flag() ? r.text() : null;
  checkTrailing(r);
  return { healthy, message };
}

// ---- Error -------------------------------------------------------------

export interface ZbrtErrorPayload {
  code: number;
  message: string;
}

export function encodeError(code: number, message: string): Buffer {
  const chunks: Buffer[] = [];
  putU32(chunks, code);
  putText(chunks, message);
  return concat(chunks);
}

export function decodeError(payload: Uint8Array): ZbrtErrorPayload {
  const r = new Reader(Buffer.from(payload));
  const code = r.u32();
  const message = r.text();
  checkTrailing(r);
  return { code, message };
}
