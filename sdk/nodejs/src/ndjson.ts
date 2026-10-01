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
  host?: string;
  port?: number;
  /** Unix socket path (`uds:<path>` — the Firecracker vsock relay UDS). */
  socketPath?: string;
  /** Guest vsock port for the relay CONNECT preamble (uds form). */
  guestPort?: number;
}

/**
 * FORKD_AGENT_TOKEN (cross-language contract §11): when set to a non-empty
 * value, every guest NDJSON connection must authenticate with
 * `{"action":"auth","token":…}` immediately after connect and only proceed
 * once `{"action":"auth","ok":true}` comes back. Unset/blank = no auth.
 */
export function agentAuthToken(): string | null {
  const token = process.env.FORKD_AGENT_TOKEN;
  return token === undefined || token.trim().length === 0 ? null : token;
}

/** Parse "host:port" (IPv6 literals allowed) or "uds:<path>[@<guestPort>]";
 * throws ValidationError. */
export function parseAddress(address: string): GuestAddress {
  if (typeof address === 'string' && address.startsWith('uds:')) {
    const rest = address.slice(4);
    const at = rest.lastIndexOf('@');
    const socketPath = at >= 0 ? rest.slice(0, at) : rest;
    const guestPort = at >= 0 ? Number.parseInt(rest.slice(at + 1), 10) : 5000;
    if (!socketPath || Number.isNaN(guestPort)) {
      throw new ValidationError('invalid guest address: expected uds:<path>[@<port>]');
    }
    return { socketPath, guestPort };
  }
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
 * immediately. `timeoutMs` is the whole read budget — for exec/eval the
 * caller passes client timeout + exec deadline + a 5 s margin so the guest's
 * own timeout error is what surfaces (Python `_guest.py` baseline).
 */
export function request(
  host: string,
  port: number,
  timeoutMs: number,
  action: Record<string, unknown>,
): Promise<NdjsonExchange> {
  const socket = net.createConnection({ host, port });
  return startExchange(socket, action, timeoutMs, { fresh: true }).then(
    (exchange) => {
      socket.end();
      return exchange;
    },
    (error) => {
      socket.destroy();
      throw error;
    },
  );
}

/**
 * 温连接池（NDJSON）：agent 的 serve 循环在一条连接上顺序承载多个请求，
 * 每请求新建 TCP 连接的握手/拆除 ≈ 0.4ms。连接私有于一次操作期间（一次
 * 活跃请求）；借出上失败 = 请求可能已在 guest 执行，绝不重试（弃用连接）；
 * 空闲 >1s 的连接直接弃用重连（ndjson 无握手，TCP connect 本身就是验证）。
 */
export class NdjsonPool {
  private readonly idle: { socket: net.Socket; lastUsed: number }[] = [];

  constructor(
    private readonly host: string,
    private readonly port: number,
  ) {}

  request(action: Record<string, unknown>, timeoutMs: number): Promise<NdjsonExchange> {
    const idle = this.idle.pop();
    if (idle && performance.now() - idle.lastUsed < 1000) {
      return startExchange(idle.socket, action, timeoutMs, { fresh: false }).then(
        (exchange) => {
          this.idle.push({ socket: idle.socket, lastUsed: performance.now() });
          return exchange;
        },
        (error) => {
          idle.socket.destroy();
          throw error;
        },
      );
    }
    if (idle) {
      idle.socket.destroy();
    }
    const socket = net.createConnection({ host: this.host, port: this.port });
    return startExchange(socket, action, timeoutMs, { fresh: true }).then(
      (exchange) => {
        this.idle.push({ socket, lastUsed: performance.now() });
        return exchange;
      },
      (error) => {
        socket.destroy();
        throw error;
      },
    );
  }

  /** 丢弃所有空闲连接（sandbox 删除/停机）。 */
  dispose(): void {
    for (const { socket } of this.idle.splice(0)) {
      socket.destroy();
    }
  }
}

/**
 * 一次 NDJSON 交换：`fresh` = 新建的 socket（在 connect 后发，可能先走
 * agent auth）；否则 = 池借出的已连接 socket（立即发，无 auth）。
 */
function startExchange(
  socket: net.Socket,
  action: Record<string, unknown>,
  timeoutMs: number,
  opts: { fresh: boolean },
): Promise<NdjsonExchange> {
  // 池化复用的前提：上一次交换的监听器必须清干净——否则每次借出都往同一
  // socket 上再挂 data/error/close 闭包（旧闭包持有各自的 responses/buffer，
  // 实测 2000 次操作 = 1.3GB 留驻且 GC 不回落）。
  socket.removeAllListeners();
  return new Promise<NdjsonExchange>((resolve, reject) => {
    const responses: Record<string, unknown>[] = [];
    let buffer = Buffer.alloc(0);
    let settled = false;
    const agentToken = agentAuthToken();
    let awaitingAuth = opts.fresh && agentToken !== null;

    const sendLine = (value: Record<string, unknown>): void => {
      socket.write(Buffer.from(JSON.stringify(value) + '\n', 'utf8'));
    };

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
        // 池化：socket 的归属归调用方（归还或销毁），这里不 end()。
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

    const begin = (): void => {
      socket.setNoDelay(true);
      if (agentToken === null) {
        sendLine(action);
      } else {
        // Agent auth first; the real action goes out only after ok=true.
        sendLine({ action: 'auth', token: agentToken });
      }
    };
    if (opts.fresh) {
      socket.once('connect', begin);
    } else {
      begin();
    }

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
        if (awaitingAuth) {
          // Intercept before the terminal-key check: the ack itself carries
          // `ok`, which would otherwise end the exchange prematurely.
          if (value && value.action === 'auth' && value.ok === true) {
            awaitingAuth = false;
            sendLine(action);
            continue;
          }
          fail(new RemoteError('agent auth failed'));
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
