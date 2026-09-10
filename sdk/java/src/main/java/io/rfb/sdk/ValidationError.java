package io.rfb.sdk;

/**
 * A request failed local (fail-closed) validation before it was sent — e.g. an
 * invalid guest path, an oversized pattern, or an out-of-range limit.
 */
public class ValidationError extends RfbError {
    private static final long serialVersionUID = 1L;

    public ValidationError(String message) {
        super(message);
    }
}
