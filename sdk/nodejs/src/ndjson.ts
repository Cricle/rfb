/**
 * forkd guest TCP NDJSON transport (PROTOCOL.md §2): one connection per
 * request, one JSON line out, response lines in until a terminal key.
 * INTERNAL — the public surface is the Sandbox class.
 */
import net from 'node:net';
import { DecodeError, RemoteError, TransportError, ValidationError } from './errors.js';
import { MAX_LINE_BYTES } from './validation.js';

/** PROTOCOL.md §2.1 terminal keys: any of these ends the response. */
const TERMINAL_KEYS = new Set([
  'exit_code',
  'pong',
  'results',
  'entries',
  'matches',
  'data',
  'content',
  'output',
  'status',
  'ok',
  'healthy',
  'done',
  'cancelled',
  'bytes_written',
]);

export interface GuestAddress {
  host: string;
  port: number;
}

/** Parse "host:port" (IPv6 literals allowed); throws ValidationError. */
export function parseAddress(address: string): GuestAddress {
  if (typeof address !== 'string' || address.length === 0) {
    throw new ValidationError('guest address must be host:port');
  }
  const idx = address.lastIndexOf(':');
  if (idx < 0) {
    throw new ValidationError('invalid guest address: expected host:port');
  }
  let host = address.slice(0, idx);
  const port = Number.parseInt(address.slice(idx + 1), 10);
  if (Number.isNaN(port)) {
    throw new ValidationError('invalid guest address port');
  }
  if (host.startsWith('[') && host.endsWith(']')) {
    host = host.slice(1, -1);
  }
  if (host.length === 0 || port < 1 || port > 65535) {
    throw new ValidationError('invalid guest address: expected host:port');
  }
  return { host, port };
}

export interface NdjsonExchange {
  responses: Record<string, unknown>[];
  last: Record<string, unknown>;
}

/**
 * Send one action object and collect JSON response lines until a terminal
 * key. Any response line carrying a string `error` raises RemoteError
 * immediately.
 */
export function request(
  host: string,
  port: number,
  timeoutMs: number,
  action: Record<string, unknown>,
): Promise<NdjsonExchange> {
  return new Promise<NdjsonExchange>((resolve, reject) => {
    const socket = net.createConnection({ host, port });
    const responses: Record<string, unknown>[] = [];
    let buffer = Buffer.alloc(0);
    let settled = false;

    const fail = (error: Error) => {
      if (!settled) {
        settled = true;
        socket.destroy();
        reject(error);
      }
    };
    const done = () => {
      if (!settled) {
        settled = true;
        socket.end();
        resolve({
          responses,
          last: responses[responses.length - 1] ?? {},
        });
      }
    };

    socket.setTimeout(timeoutMs);
    socket.on('timeout', () => fail(new TransportError('guest connection timed out')));
    socket.on('error', (error) =>
      fail(new TransportError(`guest connection failed: ${error.message}`)),
    );

    socket.on('connect', () => {
      socket.setNoDelay(true);
      socket.write(Buffer.from(JSON.stringify(action) + '\n', 'utf8'));
    });

    socket.on('data', (chunk: Buffer) => {
      buffer = Buffer.concat([buffer, chunk]);
      while (true) {
        const nl = buffer.indexOf('\n');
        if (nl < 0) {
          if (buffer.length > MAX_LINE_BYTES) {
            fail(new DecodeError(`guest response exceeded ${MAX_LINE_BYTES} bytes`));
          }
          break;
        }
        let raw = buffer.subarray(0, nl);
        buffer = buffer.subarray(nl + 1);
        while (raw.length > 0 && (raw[raw.length - 1] === 0x0a || raw[raw.length - 1] === 0x0d)) {
          raw = raw.subarray(0, raw.length - 1);
        }
        // The cap applies to newline-terminated lines too: without this a
        // single oversized line slips through and the JSON parse materializes
        // it (Python/Java/C# all reject here).
        if (raw.length > MAX_LINE_BYTES) {
          fail(new DecodeError(`guest response exceeded ${MAX_LINE_BYTES} bytes`));
          return;
        }
        if (raw.length === 0) {
          continue; // skip empty keepalive lines
        }
        let value: Record<string, unknown>;
        try {
          value = JSON.parse(raw.toString('utf8')) as Record<string, unknown>;
        } catch (error) {
          fail(new DecodeError(`invalid guest JSON: ${(error as Error).message}`));
          return;
        }
        if (value && typeof value.error === 'string') {
          fail(new RemoteError(value.error));
          return;
        }
        responses.push(value);
        if (value && Object.keys(value).some((k) => TERMINAL_KEYS.has(k))) {
          done();
          return;
        }
      }
    });

    socket.on('close', () => {
      if (!settled) {
        if (responses.length > 0) {
          done();
        } else {
          fail(new RemoteError('guest closed before response'));
        }
      }
    });
  });
}
