package io.rfb.sdk;

/**
 * Base unchecked error for all RFB SDK failures. Concrete subclasses identify
 * the failure category (see sdk/UNIFIED_API.md §7):
 * <ul>
 *   <li>{@link TransportError} — connection/read/write failure or timeout</li>
 *   <li>{@link HttpStatusError} — forkd controller returned non-2xx</li>
 *   <li>{@link DecodeError} — response/frame decoding failed (incl. strict codec)</li>
 *   <li>{@link RemoteError} — the peer reported an error</li>
 *   <li>{@link ValidationError} — local fail-closed request validation</li>
 * </ul>
 */
public class RfbError extends RuntimeException {
    private static final long serialVersionUID = 1L;

    public RfbError(String message) {
        super(message);
    }

    public RfbError(String message, Throwable cause) {
        super(message, cause);
    }
}
