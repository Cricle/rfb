package io.rfb.sdk;

/**
 * The peer reported an error: a guest {@code "error"} JSON line, a ZBRT
 * {@code Error} frame, or a controller-declared failure.
 */
public class RemoteError extends RfbError {
    private static final long serialVersionUID = 1L;

    /** ZBRT Error frame code; -1 when the error carried no numeric code. */
    private final long code;

    public RemoteError(String message) {
        super(message);
        this.code = -1;
    }

    public RemoteError(long code, String message) {
        super("remote error " + code + ": " + message);
        this.code = code;
    }

    public long getCode() {
        return code;
    }
}
