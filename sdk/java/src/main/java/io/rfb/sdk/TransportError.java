package io.rfb.sdk;

/** Connection/read/write failure or timeout at the transport level. */
public class TransportError extends RfbError {
    private static final long serialVersionUID = 1L;

    public TransportError(String message) {
        super(message);
    }

    public TransportError(String message, Throwable cause) {
        super(message, cause);
    }
}
