/** RFB Node.js SDK error taxonomy (UNIFIED_API.md §7). Catch RfbError to cover every failure class. */
export class RfbError extends Error {
  constructor(message: string) {
    super(message);
    this.name = new.target.name;
  }
}

/** Connection / read-write failure, and every timeout. */
export class TransportError extends RfbError {}

/** forkd controller answered with a non-2xx status. */
export class HttpStatusError extends RfbError {
  public readonly status: number;

  constructor(status: number, message: string) {
    super(`HTTP ${status}: ${message}`);
    this.status = status;
  }
}

/** A response or frame could not be decoded. */
export class DecodeError extends RfbError {}

/** The remote side reported an error (guest `error` line, Error frame, controller `error` field). */
export class RemoteError extends RfbError {}

/** Local fail-closed validation — raised before any network traffic. */
export class ValidationError extends RfbError {}
