package io.rfb.sdk;

import java.util.Arrays;
import java.util.Objects;

/**
 * One event of an interactive {@link GuestStream}. {@code kind} is one of
 * {@link #STARTED}, {@link #STDOUT}, {@link #STDERR}, or {@link #EXIT};
 * {@code data} carries stream bytes for stdout/stderr; {@code code} carries the
 * exit code for exit events (null when the guest did not report one).
 * Byte arrays are copied in and out so the value stays effectively immutable.
 */
public final class StreamEvent {
    public static final String STARTED = "started";
    public static final String STDOUT = "stdout";
    public static final String STDERR = "stderr";
    public static final String EXIT = "exit";

    private final String kind;
    private final byte[] data;
    private final Integer code;

    private StreamEvent(String kind, byte[] data, Integer code) {
        this.kind = kind;
        this.data = data == null ? new byte[0] : data.clone();
        this.code = code;
    }

    /** Event kind: one of the public constants on this class. */
    public String getKind() {
        return kind;
    }

    /** A copy of the payload bytes (empty for non-output events). */
    public byte[] getData() {
        return data.clone();
    }

    /** Exit code for exit events; null otherwise. */
    public Integer getCode() {
        return code;
    }

    /** Session-start marker event. */
    public static StreamEvent started() {
        return new StreamEvent(STARTED, new byte[0], null);
    }

    /** One stdout chunk. */
    public static StreamEvent stdout(byte[] data) {
        return new StreamEvent(STDOUT, data, null);
    }

    /** One stderr chunk. */
    public static StreamEvent stderr(byte[] data) {
        return new StreamEvent(STDERR, data, null);
    }

    /** Terminal event carrying the child's exit code (null when unreported). */
    public static StreamEvent exit(Integer code) {
        return new StreamEvent(EXIT, new byte[0], code);
    }

    @Override
    public boolean equals(Object other) {
        if (this == other) {
            return true;
        }
        if (!(other instanceof StreamEvent)) {
            return false;
        }
        StreamEvent that = (StreamEvent) other;
        return Objects.equals(kind, that.kind)
                && Arrays.equals(data, that.data)
                && Objects.equals(code, that.code);
    }

    @Override
    public int hashCode() {
        return Objects.hash(kind, Arrays.hashCode(data), code);
    }

    @Override
    public String toString() {
        return "StreamEvent{kind=" + kind + ", code=" + code + ", data=" + data.length + "B}";
    }
}
