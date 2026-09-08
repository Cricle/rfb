package io.rfb.sdk;

/**
 * One event of an interactive {@link GuestStream}. {@code kind} is one of
 * {@code "started"}, {@code "stdout"}, {@code "stderr"}, {@code "exit"};
 * {@code data} carries stream bytes for stdout/stderr; {@code code} carries the
 * exit code for exit events (null when the guest did not report one).
 */
public final class StreamEvent {
    public static final String STARTED = "started";
    public static final String STDOUT = "stdout";
    public static final String STDERR = "stderr";
    public static final String EXIT = "exit";

    public final String kind;
    public final byte[] data;
    public final Integer code;

    private StreamEvent(String kind, byte[] data, Integer code) {
        this.kind = kind;
        this.data = data == null ? new byte[0] : data;
        this.code = code;
    }

    public static StreamEvent started() {
        return new StreamEvent(STARTED, new byte[0], null);
    }

    public static StreamEvent stdout(byte[] data) {
        return new StreamEvent(STDOUT, data, null);
    }

    public static StreamEvent stderr(byte[] data) {
        return new StreamEvent(STDERR, data, null);
    }

    public static StreamEvent exit(Integer code) {
        return new StreamEvent(EXIT, new byte[0], code);
    }

    @Override
    public String toString() {
        return "StreamEvent{kind=" + kind + ", code=" + code + ", data=" + data.length + "B}";
    }
}
