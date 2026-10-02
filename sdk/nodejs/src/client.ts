/**
 * RFB Node.js SDK public client (UNIFIED_API.md). Mirror port of the Rust
 * `rfb::client::RfbClient`: forkd controller lifecycle over HTTP/JSON plus a
 * `Sandbox` facade over the forkd guest NDJSON and ZBRT transports.
 */
import { DecodeError, HttpStatusError, RemoteError, TransportError, ValidationError } from './errors.js';
import * as validation from './validation.js';
import { Sandbox, TRANSPORT_NDJSON, TRANSPORT_ZBRT } from './sandbox.js';
import type { SandboxInfo } from './sandbox.js';

/** create 的独立预算：快照恢复（restore+resume）可超基础 10s——超时会
 * 孤儿一个已落地的沙箱。 */
const CREATE_BUDGET_MS = 60_000;

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
  dir?: string;
  created_at_unix?: number | null;
  branched_from?: string | null;
  pause_ms?: number | null;
  diff_ms?: number | null;
  diff_physical_bytes?: number | null;
  diff_logical_bytes?: number | null;
  warning?: string | null;
  digest?: string | null;
  provenance?: unknown;
  [key: string]: unknown;
}

/** Snapshot DTO (§1): alias of {@link SnapshotSummary}; missing fields default (serde defaults). */
export type Snapshot = SnapshotSummary;

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
  // §2/§8/§10: FORKD_URL falls back to the default when unset OR blank.
  const url = process.env.FORKD_URL;
  return url === undefined || url.trim().length === 0 ? DEFAULT_URL : url;
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
   * Create an RFB SDK client (§2). Defaults resolve from env: `FORKD_URL`
   * (unset or blank → `http://127.0.0.1:8889`), `FORKD_TOKEN` (sent as
   * `Authorization: Bearer` only when non-empty) and a 10 s per-request
   * timeout covering connect + read. The timeout is validated fail-closed.
   *
   * @param options Optional overrides: `baseUrl`, `token`, `timeoutS`.
   * @throws {ValidationError} `timeoutS` is not a positive finite number, or
   *   `baseUrl` is not an http(s) URL with a host.
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

  async #send(method: string, pathAndQuery: string, body?: unknown, budgetMs?: number): Promise<HttpResponse> {
    // PROTOCOL.md §1.2: a keep-alive connection may be closed by the peer;
    // retry once for idempotent methods (GET/DELETE/PUT).
    for (let attempt = 0; attempt < 2; attempt++) {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), budgetMs ?? this.timeoutS * 1000);
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

  /**
   * All snapshots as reported by the controller (§3: `GET /v1/snapshots`).
   *
   * @returns Every snapshot (missing DTO fields follow serde defaults).
   * @throws {TransportError} Connection failure or timeout.
   * @throws {HttpStatusError} Non-2xx controller answer.
   * @throws {DecodeError} Malformed 2xx body.
   */
  async listSnapshots(): Promise<SnapshotSummary[]> {
    return this.#parseJson<SnapshotSummary[]>(this.#expectOk(await this.#send('GET', '/v1/snapshots')));
  }

  /**
   * The live sandbox registry, as attachable handles (§3).
   *
   * @returns One {@link Sandbox} per live sandbox (NDJSON transport).
   * @throws {TransportError} Connection failure or timeout.
   * @throws {HttpStatusError} Non-2xx controller answer.
   * @throws {DecodeError} Malformed 2xx body.
   */
  async listSandboxes(): Promise<Sandbox[]> {
    const infos = this.#parseJson<SandboxInfo[]>(this.#expectOk(await this.#send('GET', '/v1/sandboxes')));
    return infos.map((info) => new Sandbox(info, this, TRANSPORT_NDJSON, this.timeoutS * 1000));
  }

  /**
   * Snapshot detail: `/info` endpoint → legacy endpoint; both 404 → `null`
   * (§3). The tag is validated non-empty only and percent-encoded verbatim
   * into the path (PROTOCOL.md §1.1), so tags like "base.v2" / "snap:1" stay
   * legal.
   *
   * @param tag Snapshot tag to look up.
   * @returns The snapshot, or `null` when unknown (double 404).
   * @throws {ValidationError} Empty tag.
   * @throws {TransportError} Connection failure or timeout.
   * @throws {HttpStatusError} Non-2xx (other than the 404 fallbacks).
   * @throws {DecodeError} Malformed 2xx body.
   */
  async snapshot(tag: string): Promise<SnapshotSummary | null> {
    validation.snapshotTag(tag);
    const encoded = encodeURIComponent(tag);
    const preferred = await this.#send('GET', `/v1/snapshots/${encoded}/info`);
    if (preferred.status !== 404) {
      return this.#parseJson<SnapshotSummary>(this.#expectOk(preferred));
    }
    const legacy = await this.#send('GET', `/v1/snapshots/${encoded}`);
    if (legacy.status === 404) return null;
    return this.#parseJson<SnapshotSummary>(this.#expectOk(legacy));
  }

  /**
   * Poll every 100 ms until the snapshot is ready and bootable (§3): a
   * `failed` status raises {@link RemoteError} immediately; exceeding the
   * budget raises {@link TransportError} (timeouts are transport-class).
   *
   * @param tag Snapshot tag to wait for.
   * @param timeoutS Wait budget in seconds (default 60).
   * @returns The ready, bootable snapshot.
   * @throws {ValidationError} Empty tag or non-positive/NaN timeout.
   * @throws {RemoteError} The snapshot reports `failed`.
   * @throws {TransportError} The budget elapsed, or a poll request failed.
   * @throws {HttpStatusError} Non-2xx controller answer while polling.
   * @throws {DecodeError} Malformed 2xx body while polling.
   */
  async waitSnapshot(tag: string, timeoutS: number = DEFAULT_WAIT_TIMEOUT_S): Promise<SnapshotSummary> {
    validation.snapshotTag(tag);
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
   * Create sandboxes from a snapshot (§3). Transports: `"ndjson"` (default)
   * and `"zbrt"` — both are fully implemented; anything else raises
   * {@link ValidationError}.
   *
   * @param tag Snapshot tag to branch from.
   * @param options Optional knobs: `n` (default 1), `transport`
   *   (default `"ndjson"`), `perChildNetns`, `memoryLimitMib`, `prewarm`,
   *   `liveFork`, `hugepages` (all default false/null).
   * @returns One handle per created sandbox, over the chosen transport.
   * @throws {ValidationError} Unknown tag/transport.
   * @throws {TransportError} Connection failure or timeout.
   * @throws {HttpStatusError} Non-2xx controller answer.
   * @throws {DecodeError} Malformed 2xx body.
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
    validation.snapshotTag(tag);
    validation.transport(transport);
    // create 的快照恢复实测可超基础 10s（provider 同款结论：超时 = 孤儿
    // 一个已落地的沙箱）——create 用独立的 60s 预算。
    const result = await this.#send('POST', '/v1/sandboxes', {
      snapshot_tag: tag,
      n,
      per_child_netns: perChildNetns,
      memory_limit_mib: memoryLimitMib,
      prewarm,
      live_fork: liveFork,
      hugepages,
    }, CREATE_BUDGET_MS);
    const infos = this.#parseJson<SandboxInfo[]>(this.#expectOk(result));
    return infos.map((info) => new Sandbox(info, this, transport, this.timeoutS * 1000));
  }

  /**
   * Attach to a sandbox (§3).
   *
   * @param target A sandbox id, resolved through the controller's sandbox
   *   list (a miss raises {@link RemoteError}); or an existing {@link Sandbox}
   *   handle, attached as-is.
   * @param transport Optional explicit transport (`"ndjson"` | `"zbrt"`).
   *   Passing a `Sandbox` plus an explicit transport overrides its transport
   *   (the rest of the handle is reused); by id it selects the transport
   *   directly (default `"ndjson"`). Invalid values raise
   *   {@link ValidationError} before any network traffic.
   * @returns A `Sandbox` handle over the chosen transport.
   * @throws {ValidationError} Invalid transport or sandbox id.
   * @throws {RemoteError} The id is not in the controller's live list.
   * @throws {TransportError} The controller request failed or timed out.
   * @throws {HttpStatusError} The controller answered non-2xx.
   */
  async connect(target: string | Sandbox, transport?: string | null): Promise<Sandbox> {
    if (target instanceof Sandbox) {
      // §3: a Sandbox attaches as-is; only an explicitly passed transport
      // overrides it (mirrors the Rust ConnectTarget for &Sandbox).
      if (transport === undefined || transport === null) {
        return target;
      }
      validation.transport(transport);
      return new Sandbox(target.info, this, transport, this.timeoutS * 1000);
    }
    if (transport === undefined || transport === null) {
      transport = TRANSPORT_NDJSON;
    }
    validation.transport(transport);
    const sandboxId = target;
    validation.sandboxId(sandboxId);
    const result = await this.#send('GET', '/v1/sandboxes');
    const list = this.#parseJson<SandboxInfo[]>(this.#expectOk(result));
    const info = Array.isArray(list) ? list.find((s) => s.id === sandboxId) : undefined;
    if (!info) {
      throw new RemoteError(`sandbox not found: ${sandboxId}`);
    }
    return new Sandbox(info, this, transport, this.timeoutS * 1000);
  }

  /**
   * Attach to a sandbox with an explicit transport ("ndjson" | "zbrt").
   *
   * @param sandboxId Sandbox id to resolve.
   * @param transport Optional transport override; `null`/`undefined` keeps
   *   the `"ndjson"` default.
   * @returns A `Sandbox` handle over the chosen transport.
   * @throws {ValidationError} Invalid transport or sandbox id, before any
   *   network traffic.
   * @throws {RemoteError} The id is not in the controller's live list.
   * @throws {TransportError} The controller request failed or timed out.
   * @throws {HttpStatusError} The controller answered non-2xx.
   */
  async connectWithTransport(sandboxId: string, transport?: string | null): Promise<Sandbox> {
    return this.connect(sandboxId, transport);
  }

  /**
   * Controller-level ping for a sandbox id (§3: the ping value passes
   * through unchanged).
   *
   * @param sandboxId Sandbox id to ping.
   * @returns The controller's ping response body as parsed JSON.
   * @throws {ValidationError} Malformed sandbox id.
   * @throws {TransportError} Connection failure or timeout.
   * @throws {HttpStatusError} Non-2xx controller answer.
   * @throws {DecodeError} Malformed 2xx body.
   */
  async pingSandbox(sandboxId: string): Promise<Record<string, unknown>> {
    validation.sandboxId(sandboxId);
    const result = await this.#send('POST', `/v1/sandboxes/${sandboxId}/ping`);
    return this.#parseJson<Record<string, unknown>>(this.#expectOk(result));
  }

  /**
   * Delete a sandbox (§3: both 2xx and 404 are success).
   *
   * @param sandboxId Sandbox id to delete.
   * @throws {ValidationError} Malformed sandbox id.
   * @throws {TransportError} Connection failure or timeout.
   * @throws {HttpStatusError} Non-2xx, non-404 controller answer.
   */
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
