/**
 * RFB Node.js SDK public client (UNIFIED_API.md). Mirror port of the Rust
 * `rfb::client::RfbClient`: forkd controller lifecycle over HTTP/JSON plus a
 * `Sandbox` facade over the forkd guest NDJSON and ZBRT transports.
 */
import { DecodeError, HttpStatusError, RemoteError, TransportError, ValidationError } from './errors.js';
import * as validation from './validation.js';
import { Sandbox, TRANSPORT_NDJSON, TRANSPORT_ZBRT } from './sandbox.js';
import type { SandboxInfo } from './sandbox.js';

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
  /** Isolated network namespace per child (default false, shared tap). */
  perChildNetns?: boolean;
  /** Memory cap for each child in MiB (null = controller default). */
  memoryLimitMib?: number | null;
  /** Prewarm throwaway children (default false). */
  prewarm?: boolean;
  /** Live-fork from a running parent (default false). */
  liveFork?: boolean;
  /** Back children with hugepages (default false). */
  hugepages?: boolean;
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
    // PROTOCOL.md §1.2: a keep-alive connection may be closed by the peer;
    // retry once for idempotent methods (GET/DELETE/PUT).
    for (let attempt = 0; attempt < 2; attempt++) {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), this.timeoutS * 1000);
      try {
        const response = await fetch(this.baseUrl + pathAndQuery, {
          method,
          headers: {
            ...(this.token !== null ? { Authorization: `Bearer ${this.token}` } : {}),
            ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}),
          },
          body: body === undefined ? null : JSON.stringify(body),
          signal: controller.signal,
        });
        clearTimeout(timer);
        const text = await response.text().catch(() => '');
        return { status: response.status, body: text };
      } catch (error) {
        clearTimeout(timer);
        if (attempt === 0 && (method === 'GET' || method === 'DELETE' || method === 'PUT')) {
          continue; // stale keep-alive: retry once
        }
        const e = error as Error & { cause?: { message?: string } };
        throw new TransportError(`forkd request failed: ${e.cause?.message ?? e.message}`);
      }
    }
    // Unreachable: the loop returns on success or throws on error.
    throw new Error('unreachable');
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

  /** Strict response parse: a malformed 2xx body is a DecodeError (§7), not SyntaxError. */
  #parseJson<T>(text: string): T {
    try {
      return JSON.parse(text) as T;
    } catch (error) {
      throw new DecodeError(`invalid forkd response json: ${(error as Error).message}`);
    }
  }

  /** GET /v1/snapshots. */
  async listSnapshots(): Promise<SnapshotSummary[]> {
    return this.#parseJson<SnapshotSummary[]>(this.#expectOk(await this.#send('GET', '/v1/snapshots')));
  }

  /** GET /v1/sandboxes — the live sandbox registry, as attachable handles. */
  async listSandboxes(): Promise<Sandbox[]> {
    const infos = this.#parseJson<SandboxInfo[]>(this.#expectOk(await this.#send('GET', '/v1/sandboxes')));
    return infos.map((info) => new Sandbox(info, this, TRANSPORT_NDJSON, this.timeoutS * 1000));
  }

  /** Snapshot detail: /info → legacy endpoint; both 404 → null. */
  async snapshot(tag: string): Promise<SnapshotSummary | null> {
    validation.sandboxId(tag);
    const preferred = await this.#send('GET', `/v1/snapshots/${tag}/info`);
    if (preferred.status !== 404) {
      return this.#parseJson<SnapshotSummary>(this.#expectOk(preferred));
    }
    const legacy = await this.#send('GET', `/v1/snapshots/${tag}`);
    if (legacy.status === 404) return null;
    return this.#parseJson<SnapshotSummary>(this.#expectOk(legacy));
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
    const {
      n = 1,
      transport = TRANSPORT_NDJSON,
      perChildNetns = false,
      memoryLimitMib = null,
      prewarm = false,
      liveFork = false,
      hugepages = false,
    } = options;
    validation.sandboxId(tag);
    if (transport !== TRANSPORT_NDJSON && transport !== TRANSPORT_ZBRT) {
      throw new ValidationError(`invalid transport: ${transport}`);
    }
    const result = await this.#send('POST', '/v1/sandboxes', {
      snapshot_tag: tag,
      n,
      per_child_netns: perChildNetns,
      memory_limit_mib: memoryLimitMib,
      prewarm,
      live_fork: liveFork,
      hugepages,
    });
    const infos = this.#parseJson<SandboxInfo[]>(this.#expectOk(result));
    return infos.map((info) => new Sandbox(info, this, transport, this.timeoutS * 1000));
  }

  /**
   * Attach to a sandbox by id (resolved via the controller's sandbox list),
   * or pass an existing `Sandbox` to reuse it as-is — its transport is
   * preserved (attaching an existing handle never resets it).
   */
  async connect(target: string | Sandbox): Promise<Sandbox> {
    if (target instanceof Sandbox) {
      return target;
    }
    const sandboxId = target;
    validation.sandboxId(sandboxId);
    const result = await this.#send('GET', '/v1/sandboxes');
    const list = this.#parseJson<SandboxInfo[]>(this.#expectOk(result));
    const info = Array.isArray(list) ? list.find((s) => s.id === sandboxId) : undefined;
    if (!info) {
      throw new RemoteError('sandbox not found');
    }
    return new Sandbox(info, this, TRANSPORT_NDJSON, this.timeoutS * 1000);
  }

  /** Attach to a sandbox with an explicit transport ("ndjson" | "zbrt"). */
  async connectWithTransport(sandboxId: string, transport?: string | null): Promise<Sandbox> {
    // Fail closed before any network traffic (§7): validate the transport
    // name first, then resolve the sandbox.
    if (transport !== undefined && transport !== null) {
      if (transport !== TRANSPORT_NDJSON && transport !== TRANSPORT_ZBRT) {
        throw new ValidationError(`invalid transport: ${transport}`);
      }
      const sandbox = await this.connect(sandboxId);
      return new Sandbox(sandbox.info, this, transport, this.timeoutS * 1000);
    }
    return this.connect(sandboxId);
  }

  async pingSandbox(sandboxId: string): Promise<Record<string, unknown>> {
    validation.sandboxId(sandboxId);
    const result = await this.#send('POST', `/v1/sandboxes/${sandboxId}/ping`);
    return this.#parseJson<Record<string, unknown>>(this.#expectOk(result));
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

// Public surface: errors and sandbox value types must be importable from the
// package root (README/quickstart use `import { RfbClient, RfbError }`).
export * from './errors.js';
export * from './sandbox.js';
