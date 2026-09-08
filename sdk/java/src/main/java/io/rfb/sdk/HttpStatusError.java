package io.rfb.sdk;

/**
 * The forkd controller returned a non-2xx HTTP status. The message carries the
 * controller-provided {@code error} JSON field when present, otherwise the
 * first 1024 characters of the response body.
 */
public class HttpStatusError extends RfbError {
    private static final long serialVersionUID = 1L;

    private final int status;

    public HttpStatusError(int status, String message) {
        super("forkd returned " + status + ": " + message);
        this.status = status;
    }

    /** HTTP status code returned by the controller. */
    public int getStatus() {
        return status;
    }
}
