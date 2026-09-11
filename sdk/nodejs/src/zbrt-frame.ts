/**
 * ZBRT v1 wire frame (PROTOCOL.md §3.1, mirroring
 * `rfb/src/zeroboot_protocol.rs` and the Java ZbrtFrame). Header: magic
 * "ZBRT", version 1, kind, flags (u16 BE, must be 0), request_id (16 bytes),
 * payload_len (u32 BE, ≤ 16 MiB). INTERNAL.
 */
import { DecodeError } from './errors.js';

export const HEADER_LEN = 28;
export const MAX_PAYLOAD = 16 * 1024 * 1024;
export const MAGIC = Buffer.from('ZBRT', 'ascii');
export const VERSION = 1;

export const KIND_HELLO = 1;
export const KIND_HELLO_ACK = 2;
export const KIND_EXECUTE = 3;
export const KIND_OUTPUT = 4;
export const KIND_EXIT = 5;
export const KIND_CANCEL = 6;
export const KIND_CANCEL_ACK = 7;
export const KIND_FS = 8;
export const KIND_FS_RESULT = 9;
export const KIND_HEALTH = 10;
export const KIND_HEALTH_ACK = 11;
export const KIND_ERROR = 12;

function checkKind(kind: number): number {
  if (kind < 1 || kind > 13) {
    throw new DecodeError(`unknown frame kind: ${kind}`);
  }
  return kind;
}

export class ZbrtFrame {
  public readonly kind: number;
  public readonly flags: number;
  public readonly requestId: Buffer;
  public readonly payload: Buffer;

  constructor(kind: number, flags: number, requestId: Uint8Array, payload: Uint8Array) {
    if (requestId.length !== 16) {
      throw new DecodeError('request_id must be 16 bytes');
    }
    if (payload.length > MAX_PAYLOAD) {
      throw new DecodeError(`payload too large: ${payload.length}`);
    }
    checkKind(kind);
    this.kind = kind;
    this.flags = flags;
    this.requestId = Buffer.from(requestId);
    this.payload = Buffer.from(payload);
  }

  /** Static decode entry (mirrors the Java ZbrtFrame.decode). */
  static decode(bytes: Uint8Array): ZbrtFrame {
    return decode(bytes);
  }

  encode(): Buffer {
    if (this.flags !== 0 || this.payload.length > MAX_PAYLOAD) {
      throw new DecodeError('invalid frame');
    }
    const out = Buffer.alloc(HEADER_LEN + this.payload.length);
    MAGIC.copy(out, 0);
    out[4] = VERSION;
    out[5] = this.kind;
    out[6] = (this.flags >>> 8) & 0xff;
    out[7] = this.flags & 0xff;
    this.requestId.copy(out, 8);
    const len = this.payload.length;
    out[24] = (len >>> 24) & 0xff;
    out[25] = (len >>> 16) & 0xff;
    out[26] = (len >>> 8) & 0xff;
    out[27] = len & 0xff;
    this.payload.copy(out, HEADER_LEN);
    return out;
  }
}

function payloadLength(header: Buffer): number {
  const len = header.readUInt32BE(24);
  if (len > MAX_PAYLOAD) {
    throw new DecodeError(`payload too large: ${len}`);
  }
  return len;
}

function checkHeaderMagic(header: Buffer): void {
  if (
    header[0] !== MAGIC[0] ||
    header[1] !== MAGIC[1] ||
    header[2] !== MAGIC[2] ||
    header[3] !== MAGIC[3] ||
    header[4] !== VERSION
  ) {
    throw new DecodeError('invalid magic or version');
  }
}

function decodeHeaderAndPayload(header: Buffer, payload: Buffer): ZbrtFrame {
  checkHeaderMagic(header);
  // Header length is validated before this is called (HEADER_LEN bytes).
  const kind = checkKind(header[5]!);
  const flags = header.readUInt16BE(6);
  if (flags !== 0) {
    throw new DecodeError(`unsupported frame flags: ${flags}`);
  }
  const requestId = Buffer.from(header.subarray(8, 24));
  return new ZbrtFrame(kind, flags, requestId, payload);
}

/** Decode one frame from a complete buffer (header + payload). */
export function decode(bytes: Uint8Array): ZbrtFrame {
  const buf = Buffer.from(bytes);
  if (buf.length < HEADER_LEN) {
    throw new DecodeError('truncated frame header');
  }
  const header = buf.subarray(0, HEADER_LEN);
  const len = payloadLength(header);
  if (buf.length < HEADER_LEN + len) {
    throw new DecodeError('truncated frame payload');
  }
  return decodeHeaderAndPayload(header, buf.subarray(HEADER_LEN, HEADER_LEN + len));
}

export interface FrameReader {
  /** Next complete frame, or null on stream end. Throws on decode errors. */
  next(): Promise<ZbrtFrame | null>;
}

/**
 * Async frame reader over a stream: strict header validation, 16 MiB payload
 * cap, exact-length payload reads. A dropped read leaves the stream to the
 * caller (higher layers mark such sessions desynced).
 */
export function frameReader(stream: {
  on(event: 'data', listener: (chunk: Buffer) => void): unknown;
  on(event: 'error', listener: (error: Error) => void): unknown;
  on(event: 'end', listener: () => void): unknown;
}): FrameReader {
  let buffer = Buffer.alloc(0);
  const pending: ZbrtFrame[] = [];
  let failure: Error | null = null;
  let done = false;
  // A queue of waiters, not a single slot: concurrent next() callers (e.g. a
  // stream reader and a cancel round-trip) must all be woken; a single-slot
  // resolver loses every wake-up but the last and hangs the others.
  const waiters: (() => void)[] = [];

  const wake = () => {
    const ready = waiters.splice(0);
    for (const resolve of ready) resolve();
  };

  stream.on('data', (chunk: Buffer) => {
    buffer = Buffer.concat([buffer, chunk]);
    while (true) {
      if (buffer.length < HEADER_LEN) break;
      const len = buffer.readUInt32BE(24);
      if (len > MAX_PAYLOAD) {
        failure = new DecodeError(`payload too large: ${len}`);
        done = true;
        wake();
        return;
      }
      if (buffer.length < HEADER_LEN + len) break;
      const raw = buffer.subarray(0, HEADER_LEN + len);
      buffer = buffer.subarray(HEADER_LEN + len);
      try {
        pending.push(decode(raw));
      } catch (e) {
        failure = e as Error;
        done = true;
        wake();
        return;
      }
      wake();
    }
  });
  stream.on('error', (e: Error) => {
    failure = failure ?? e;
    done = true;
    wake();
  });
  stream.on('end', () => {
    done = true;
    wake();
  });

  return {
    async next(): Promise<ZbrtFrame | null> {
      while (true) {
        if (pending.length > 0) return pending.shift() as ZbrtFrame;
        if (failure !== null) throw failure;
        if (done) {
          if (buffer.length > 0) {
            throw new DecodeError('truncated frame at end of stream');
          }
          return null;
        }
        await new Promise<void>((resolve) => {
          waiters.push(resolve);
        });
      }
    },
  };
}
