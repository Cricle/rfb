/**
 * Local (fail-closed) request validation mirroring PROTOCOL.md §2.3 and
 * `rfb/src/guest/limits.rs`. All sizes are UTF-8 byte lengths. Not public API.
 */
import { ValidationError } from './errors.js';

export const TRANSPORT_NDJSON = 'ndjson';
export const TRANSPORT_ZBRT = 'zbrt';

export const MAX_GUEST_PATH_BYTES = 4096;
export const MAX_GUEST_PATTERN_BYTES = 1024;
export const MAX_GUEST_RESULTS = 1000;
export const MAX_GUEST_RESULT_BYTES = 50 * 1024;
export const MAX_GUEST_CODE_BYTES = 1024 * 1024;
export const MAX_LINE_BYTES = 1024 * 1024;
// ZBRT v1 caps: argc fits one header byte, payloads are u32-bounded and the
// reference guest enforces a 16 MiB cap on every frame payload (§8).
export const MAX_ZBRT_ARGC = 255;
export const MAX_ZBRT_PAYLOAD_BYTES = 16 * 1024 * 1024;

function isWorkspacePath(path: string): boolean {
  return path === '/workspace' || path.startsWith('/workspace/');
}

function badBasePath(path: string): boolean {
  return (
    typeof path !== 'string' ||
    path.length === 0 ||
    Buffer.byteLength(path, 'utf8') > MAX_GUEST_PATH_BYTES ||
    path.includes('\0') ||
    path.includes('\\') ||
    path.split('/').some((segment) => segment === '..')
  );
}

/** Structured fs path (ls/find/grep): relative, or absolute under /workspace. */
export function fsPath(path: string): void {
  if (badBasePath(path) || (path.startsWith('/') && !isWorkspacePath(path))) {
    throw new ValidationError(
      'invalid guest fs path: must be a non-empty, relative, non-escaping guest path',
    );
  }
}

/** File path (read/write targets and eval/stream cwd): relative or absolute non-escaping. */
export function filePath(path: string): void {
  if (badBasePath(path) || (path.length >= 2 && path[1] === ':')) {
    throw new ValidationError(
      'invalid guest file path: must be a non-empty, non-escaping guest path',
    );
  }
}

/** find/grep pattern: non-empty, no NUL, ≤ 1024 bytes. */
export function pattern(value: string): void {
  if (typeof value !== 'string' || value.length === 0 || value.includes('\0')) {
    throw new ValidationError('invalid guest pattern: must be non-empty and NUL-free');
  }
  if (Buffer.byteLength(value, 'utf8') > MAX_GUEST_PATTERN_BYTES) {
    throw new ValidationError(`guest pattern exceeds ${MAX_GUEST_PATTERN_BYTES} bytes`);
  }
}

/** Count limit (max_results / max_bytes): must be > 0 and ≤ max. */
export function limit(value: number, max: number): void {
  if (!Number.isFinite(value) || value <= 0 || value > max) {
    throw new ValidationError(`guest limit exceeded: value must be > 0 and <= ${max}`);
  }
}

/** Payload size limit (write data / eval code). */
export function payloadSize(value: number, max: number): void {
  if (value > max) {
    throw new ValidationError(`guest payload exceeds ${max} bytes`);
  }
}

/** eval code: non-empty after trimming whitespace, ≤ 1 MiB. */
export function evalCode(code: string): void {
  if (typeof code !== 'string' || code.trim().length === 0) {
    throw new ValidationError('eval code must not be empty');
  }
  if (Buffer.byteLength(code, 'utf8') > MAX_GUEST_CODE_BYTES) {
    throw new ValidationError(`eval code exceeds ${MAX_GUEST_CODE_BYTES} bytes`);
  }
}

/**
 * forkd snapshot tag used in URL paths: only non-empty is required (PROTOCOL.md
 * §1.1 — tags are percent-encoded verbatim, unlike sandbox ids which are
 * restricted to `[A-Za-z0-9_-]`).
 */
export function snapshotTag(tag: string): void {
  if (typeof tag !== 'string' || tag.length === 0) {
    throw new ValidationError('snapshot tag must not be empty');
  }
}

/** forkd sandbox id used in URL paths: non-empty, ≤ 128, only [A-Za-z0-9_-]. */
export function sandboxId(id: string): void {
  if (
    typeof id !== 'string' ||
    id.length === 0 ||
    id.length > 128 ||
    !/^[A-Za-z0-9_-]+$/.test(id)
  ) {
    throw new ValidationError('invalid forkd sandbox id');
  }
}

/** exec argv: non-empty array of strings. */
export function argv(args: readonly string[]): void {
  if (!Array.isArray(args) || args.length === 0 || args.some((arg) => typeof arg !== 'string')) {
    throw new ValidationError('args must be a non-empty string array');
  }
}

/** Guest transport name (§3): "ndjson" | "zbrt". */
export function transport(value: string): void {
  if (value !== TRANSPORT_NDJSON && value !== TRANSPORT_ZBRT) {
    throw new ValidationError(`invalid transport: ${value}`);
  }
}

/** exec timeout: positive finite seconds. */
export function timeoutS(value: number): void {
  if (typeof value !== 'number' || !Number.isFinite(value) || value <= 0) {
    throw new ValidationError('exec timeout must be a positive, finite number of seconds');
  }
}

/**
 * ZBRT v1 encodes argc in one byte: reject locally (fail closed, zero frames
 * — not even the TCP connect) instead of leaking a codec error after the
 * connection is already open. Mirrors Python `validate_zbrt_args`.
 */
export function zbrtArgs(args: readonly string[]): void {
  if (args.length > MAX_ZBRT_ARGC) {
    throw new ValidationError(`argv exceeds the ${MAX_ZBRT_ARGC}-argument ZBRT limit`);
  }
}
