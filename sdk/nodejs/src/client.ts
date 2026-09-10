/**
 * RFB Node.js SDK public client (UNIFIED_API.md). Mirror port of the Rust
 * `rfb::client::RfbClient`: forkd controller lifecycle over HTTP/JSON plus a
 * `Sandbox` facade over the forkd guest NDJSON and ZBRT transports.
 */
import { HttpStatusError, RemoteError, TransportError, ValidationError } from './errors.js';
import * as validation from './validation.js';
import { Sandbox, SandboxInfo, TRANSPORT_NDJSON, TRANSPORT_ZBRT } from './sandbox.js';

const DEFAULT_URL = 'http://127.0.0.1:8889';
const DEFAULT_TIMEOUT_S = 10.0;
const DEFAULT_WAIT_TIMEOUT_S = 60;

export interface RfbClientOptions {
  baseUrl?: string;
  token?: string | null;
  timeoutS?: number;
}

export interface SnapshotSummary {
  tag?: string;
  status?: string;
  bootable?: boolean;
  [key: string]: unknown;
}

export interface CreateSandboxOptions {
  n?: number;
  transport?: string;
}

function envUrl(): string {
  const url = process.env.FORKD_URL;
  return url === undefined || url.length === 0 ? DEFAULT_URL : url;
}

function envToken(): string | null {
  const token = process.env.FORKD_TOKEN;
  return token === undefined || token.trim().length === 0 ? null : token;
}

interface HttpResponse {
  status: number;
  body: string;
}

export class RfbClient {
  public readonly baseUrl: string;
  public readonly token: string | null;
  public readonly timeoutS: number;

  /**
   * Defaults resolve from FORKD_URL / FORKD_TOKEN and 10 seconds.
   */
  constructor(options: RfbClientOptions = {}) {
    const baseUrl = options.baseUrl ?? envUrl();
    const token = options.token !== undefined ? options.token : envToken();
    const timeoutS = options.timeoutS ?? DEFAULT_TIMEOUT_S;
    if (typeof timeoutS !== 'number' || !Number.isFinite(timeoutS) || timeoutS <= 0) {
      throw new ValidationError('timeout must be a positive, finite number of seconds');
    }
    let parsed: URL;
    try {
      parsed = new URL(baseUrl);
    } catch {
      throw new ValidationError('forkd URL must include http(s) scheme and host');
    }
    if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
      throw new ValidationError('forkd URL must include http(s) scheme and host');
    }
    this.baseUrl = baseUrl.replace(/\/+$/, '');
    this.token = token === null || token.trim().length === 0 ? null : token;
    this.timeoutS = timeoutS;
  }

  async #send(method: string, pathAndQuery: string, body?: unknown): Promise<HttpResponse> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutS * 1000);
    let response: Response;
    try {
      response = await fetch(this.baseUrl + pathAndQuery, {
        method,
        headers: {
          ...(this.token !== null ? { Authorization: `Bearer ${this.token}` } : {}),
          ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}),
        },
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: controller.signal,
      });
    } catch (error) {
      clearTimeout(timer);
      const e = error as Error & { cause?: { message?: string } };
      throw new TransportError(`forkd request failed: ${e.cause?.message ?? e.message}`);
    }
    clearTimeout(timer);
    const text = await response.text().catch(() => '');
    return { status: response.status, body: text };
  }

  #expectOk(result: HttpResponse): string {
    if (result.status < 200 || result.status > 299) {
      let message = result.body.slice(0, 1024);
      try {
        const node = JSON.parse(result.body) as Record<string, unknown>;
        if (node && typeof node.error === 'string') message = node.error;
      } catch {
        // fall through to raw body
      }
      throw new HttpStatusError(result.status, message);
    }
    return result.body;
  }

  /** GET /v1/snapshots. */
  async listSnapshots(): Promise<SnapshotSummary[]> {
    return JSON.parse(this.#expectOk(await this.#send('GET', '/v1/snapshots')));
  }

  /** Snapshot detail: /info → legacy endpoint; both 404 → null. */
  async snapshot(tag: string): Promise<SnapshotSummary | null> {
    validation.sandboxId(tag);
    const preferred = await this.#send('GET', `/v1/snapshots/${tag}/info`);
    if (preferred.status !== 404) {
      return JSON.parse(this.#expectOk(preferred));
    }
    const legacy = await this.#send('GET', `/v1/snapshots/${tag}`);
    if (legacy.status === 404) return null;
    return JSON.parse(this.#expectOk(legacy));
  }

  /** Poll every 100 ms until status=ready and bootable=true; failed → RemoteError. */
  async waitSnapshot(tag: string, timeoutS: number = DEFAULT_WAIT_TIMEOUT_S): Promise<SnapshotSummary> {
    validation.sandboxId(tag);
    validation.timeoutS(timeoutS);
    const deadline = Date.now() + timeoutS * 1000;
    while (true) {
      const snapshots = await this.listSnapshots();
      const found = snapshots.find((s) => s.tag === tag);
      if (found) {
        if (String(found.status).toLowerCase() === 'failed') {
          throw new RemoteError(`snapshot ${tag} failed`);
        }
        if (String(found.status).toLowerCase() === 'ready' && found.bootable === true) {
          return found;
        }
      }
      if (Date.now() >= deadline) {
        throw new TransportError(`snapshot ${tag} not ready within ${timeoutS}s`);
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }

  /**
   * Create sandboxes from a snapshot. Transports: "ndjson" (default) and
   * "zbrt" — both are fully implemented; anything else raises ValidationError.
   */
  async createSandbox(tag: string, options: CreateSandboxOptions = {}): Promise<Sandbox[]> {
    const { n = 1, transport = TRANSPORT_NDJSON } = options;
    validation.sandboxId(tag);
    if (transport !== TRANSPORT_NDJSON && transport !== TRANSPORT_ZBRT) {
      throw new ValidationError(`invalid transport: ${transport}`);
    }
    const result = await this.#send('POST', '/v1/sandboxes', {
      snapshot_tag: tag,
      n,
      per_child_netns: false,
      memory_limit_mib: null,
      prewarm: false,
      live_fork: false,
      hugepages: false,
    });
    const infos = JSON.parse(this.#expectOk(result)) as SandboxInfo[];
    return infos.map((info) => new Sandbox(info, this, transport, this.timeoutS * 1000));
  }

  /** Attach to a sandbox by id (resolved via the controller's sandbox list). */
  async connect(sandboxId: string): Promise<Sandbox> {
    validation.sandboxId(sandboxId);
    const result = await this.#send('GET', '/v1/sandboxes');
    const list = JSON.parse(this.#expectOk(result)) as SandboxInfo[];
    const info = Array.isArray(list) ? list.find((s) => s.id === sandboxId) : undefined;
    if (!info) {
      throw new RemoteError('sandbox not found');
    }
    return new Sandbox(info, this, TRANSPORT_NDJSON, this.timeoutS * 1000);
  }

  /** Attach to a sandbox with an explicit transport ("ndjson" | "zbrt"). */
  async connectWithTransport(sandboxId: string, transport?: string | null): Promise<Sandbox> {
    const sandbox = await this.connect(sandboxId);
    if (transport !== undefined && transport !== null) {
      if (transport !== TRANSPORT_NDJSON && transport !== TRANSPORT_ZBRT) {
        throw new ValidationError(`invalid transport: ${transport}`);
      }
      return new Sandbox(sandbox.info, this, transport, this.timeoutS * 1000);
    }
    return sandbox;
  }

  async pingSandbox(sandboxId: string): Promise<Record<string, unknown>> {
    validation.sandboxId(sandboxId);
    const result = await this.#send('POST', `/v1/sandboxes/${sandboxId}/ping`);
    return JSON.parse(this.#expectOk(result));
  }

  /** Delete a sandbox; both 2xx and 404 are success. */
  async deleteSandbox(sandboxId: string): Promise<void> {
    validation.sandboxId(sandboxId);
    const result = await this.#send('DELETE', `/v1/sandboxes/${sandboxId}`);
    if (result.status === 404 || (result.status >= 200 && result.status <= 299)) {
      return;
    }
    throw new HttpStatusError(result.status, result.body.slice(0, 1024));
  }
}

export { Sandbox };
