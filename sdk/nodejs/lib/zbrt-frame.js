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

function checkKind(kind) {
  if (kind < 1 || kind > 13) {
    throw new DecodeError(`unknown frame kind: ${kind}`);
  }
  return kind;
}

/** Immutable ZBRT frame value. Throws on invalid kind or request_id length. */
export class ZbrtFrame {
  constructor(kind, flags, requestId, payload) {
    if (requestId === null || requestId === undefined || requestId.length !== 16) {
      throw new DecodeError('request_id must be 16 bytes');
    }
    checkKind(kind);
    this.kind = kind;
    this.flags = flags;
    this.requestId = Buffer.from(requestId);
    this.payload = Buffer.from(payload);
  }

  encode() {
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

function payloadLength(header) {
  const len = header.readUInt32BE(24);
  if (len > MAX_PAYLOAD) {
    throw new DecodeError(`payload too large: ${len}`);
  }
  return len;
}

function checkHeaderMagic(header) {
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

function decodeHeaderAndPayload(header, payload) {
  checkHeaderMagic(header);
  const kind = checkKind(header[5]);
  const flags = header.readUInt16BE(6);
  if (flags !== 0) {
    throw new DecodeError(`unsupported frame flags: ${flags}`);
  }
  const requestId = Buffer.from(header.subarray(8, 24));
  return new ZbrtFrame(kind, flags, requestId, payload);
}

/** Decode one frame from a complete buffer (header + payload). */
export function decode(bytes) {
  if (bytes.length < HEADER_LEN) {
    throw new DecodeError('truncated frame header');
  }
  const header = bytes.subarray(0, HEADER_LEN);
  const len = payloadLength(header);
  if (bytes.length < HEADER_LEN + len) {
    throw new DecodeError('truncated frame payload');
  }
  return decodeHeaderAndPayload(header, bytes.subarray(HEADER_LEN, HEADER_LEN + len));
}

/**
 * Async frame reader over a stream: strict header validation, 16 MiB payload
 * cap, exact-length payload reads. Cancellation-safe: a dropped read leaves
 * the stream to the caller (the SDK marks such sessions desynced).
 */
export function frameReader(stream) {
  let buffer = Buffer.alloc(0);
  const pending = [];
  let error = null;
  let done = false;
  let notify = null;

  const wake = () => {
    const n = notify;
    notify = null;
    n?.();
  };

  stream.on('data', (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    // Extract as many complete frames as the buffer holds.
    while (true) {
      if (buffer.length < HEADER_LEN) break;
      const len = buffer.readUInt32BE(24);
      if (len > MAX_PAYLOAD) {
        error = new DecodeError(`payload too large: ${len}`);
        done = true;
        wake();
        return;
      }
      if (buffer.length < HEADER_LEN + len) break;
      const frame = buffer.subarray(0, HEADER_LEN + len);
      buffer = buffer.subarray(HEADER_LEN + len);
      try {
        const decoded = decode(frame);
        pending.push(decoded);
      } catch (e) {
        error = e;
        done = true;
        wake();
        return;
      }
      wake();
    }
  });
  stream.on('error', (e) => {
    error = error ?? e;
    done = true;
    wake();
  });
  stream.on('end', () => {
    done = true;
    wake();
  });

  return {
    /** Next complete frame, or null on stream end. Throws on decode errors. */
    async next() {
      while (true) {
        if (pending.length > 0) return pending.shift();
        if (error !== null) throw error;
        if (done) {
          if (buffer.length > 0) {
            throw new DecodeError('truncated frame at end of stream');
          }
          return null;
        }
        await new Promise((resolve) => {
          notify = resolve;
        });
      }
    },
  };
}
