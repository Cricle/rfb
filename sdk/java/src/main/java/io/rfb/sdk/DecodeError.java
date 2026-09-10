package io.rfb.sdk;

/** A response could not be decoded: invalid JSON, malformed frame, trailing bytes. */
public class DecodeError extends RfbError {
    private static final long serialVersionUID = 1L;

    public DecodeError(String message) {
        super(message);
    }

    public DecodeError(String message, Throwable cause) {
        super(message, cause);
    }
}
