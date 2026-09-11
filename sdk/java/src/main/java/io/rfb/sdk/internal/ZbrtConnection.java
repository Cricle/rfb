package io.rfb.sdk.internal;

import io.rfb.sdk.DecodeError;
import io.rfb.sdk.RemoteError;
import io.rfb.sdk.TransportError;

import java.io.BufferedInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.security.SecureRandom;
import java.time.Duration;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;

/**
 * One ZBRT v1 session over a TCP socket (PROTOCOL.md §3.4, mirroring
 * {@code zeroboot_connection.rs} semantics): fresh 128-bit request id per
 * request, Output frames strictly before exactly one terminal frame, idempotent
 * Cancel with empty-payload CancelAck, Error frames raise {@link RemoteError}.
 * INTERNAL.
 */
public final class ZbrtConnection implements AutoCloseable {
    /** ZBRT V1 wire capability vocabulary (single source of truth in the Rust crate). */
    public static final List<String> V1_CAPABILITIES =
            Collections.unmodifiableList(Arrays.asList(
                    "execute", "stream", "deadline", "health", "cancel", "filesystem"));

    /** Shared per-process source: SecureRandom.nextBytes is thread-safe. */
    private static final SecureRandom RANDOM = new SecureRandom();

    private final Socket socket;
    private final InputStream in;
    private final java.io.OutputStream out;

    public ZbrtConnection(InetSocketAddress address, Duration timeout) {
        this.socket = GuestNdjson.connect(address, timeout);
        try {
            this.in = new BufferedInputStream(socket.getInputStream());
            this.out = socket.getOutputStream();
        } catch (IOException e) {
            try {
                socket.close();
            } catch (IOException ignored) {
                // best effort
            }
            throw new TransportError("zbrt stream setup failed: " + e.getMessage(), e);
        }
    }

    /** Fresh 128-bit request id, generated per request. */
    public byte[] newRequestId() {
        byte[] id = new byte[16];
        RANDOM.nextBytes(id);
        return id;
    }

    /** Optional Hello handshake; the guest is auto-ready without it. */
    public ZbrtCodec.HelloAck hello(String clientName) {
        byte[] id = newRequestId();
        writeFrame(new ZbrtFrame(ZbrtFrame.KIND_HELLO, 0, id,
                ZbrtCodec.encodeHello(clientName, V1_CAPABILITIES)));
        ZbrtFrame frame = readReply(id);
        if (frame.kind() == ZbrtFrame.KIND_HELLO_ACK) {
            return ZbrtCodec.decodeHelloAck(frame.payload());
        }
        throw unexpected(frame, "HelloAck");
    }

    /**
     * Run one command: collect Output frames (0=stdout, 1=stderr) until exactly
     * one terminal frame — Exit (completed/cancelled) or Error (raise).
     * {@code timeoutMs} is sent on the wire as the guest-side deadline; a read
     * stall at the connection timeout is a transport-level failure
     * ({@link TransportError}) — the client never cancels on its own (the
     * Rust baseline surfaces timeouts the same way).
     */
    public Exec execute(List<String> argv, String cwd, byte[] stdin, long timeoutMs) {
        byte[] id = newRequestId();
        writeFrame(new ZbrtFrame(ZbrtFrame.KIND_EXECUTE, 0, id,
                ZbrtCodec.encodeExecute(argv, cwd, stdin == null ? new byte[0] : stdin, timeoutMs)));
        ByteArrayOutputStream stdoutBuf = new ByteArrayOutputStream();
        ByteArrayOutputStream stderrBuf = new ByteArrayOutputStream();
        while (true) {
            ZbrtFrame frame = ZbrtFrame.decode(in);
            requireId(frame, id);
            if (frame.kind() == ZbrtFrame.KIND_OUTPUT) {
                ZbrtCodec.Output output = ZbrtCodec.decodeOutput(frame.payload());
                if (output.stream() == 0) {
                    stdoutBuf.write(output.data(), 0, output.data().length);
                } else {
                    stderrBuf.write(output.data(), 0, output.data().length);
                }
            } else if (frame.kind() == ZbrtFrame.KIND_EXIT) {
                ZbrtCodec.Exit exit = ZbrtCodec.decodeExit(frame.payload());
                return new Exec(exit.code(), stdoutBuf.toByteArray(), stderrBuf.toByteArray(),
                        exit.signal(), false);
            } else if (frame.kind() == ZbrtFrame.KIND_ERROR) {
                throw errorFrame(frame);
            } else {
                throw unexpected(frame, "Output/Exit");
            }
        }
    }

    /** Idempotent cancel; expects the empty-payload CancelAck. */
    public void cancel(String reason, byte[] target16) {
        if (target16 != null && target16.length != 16) {
            throw new DecodeError("cancel target must be 16 bytes");
        }
        ZbrtFrame frame = cancelRoundTrip(reason, target16);
        if (frame.payload().length != 0) {
            throw new DecodeError("CancelAck payload must be empty");
        }
    }

    /** Health round-trip; the guest reports healthy=true, message="ready". */
    public ZbrtCodec.Health health() {
        ZbrtFrame frame = roundTrip(ZbrtFrame.KIND_HEALTH,
                ZbrtCodec.encodeHealth(true, null), ZbrtFrame.KIND_HEALTH_ACK, "HealthAck");
        return ZbrtCodec.decodeHealth(frame.payload());
    }

    /** Route one filesystem RPC (opcodes 1=ls 2=find 3=grep 4=read 5=write). */
    public byte[] fs(int op, String path, byte[] jsonArgs) {
        return roundTrip(ZbrtFrame.KIND_FS, ZbrtCodec.encodeFs(op, path, jsonArgs),
                ZbrtFrame.KIND_FS_RESULT, "FsResult").payload();
    }

    /**
     * Start an interactive turn: send Execute and return a reader that yields
     * Output/Exit frames. Only one turn may be active per connection.
     */
    public ZbrtStreamSession openStreamSession(List<String> argv, String cwd, byte[] stdin, long timeoutMs) {
        byte[] id = newRequestId();
        writeFrame(new ZbrtFrame(ZbrtFrame.KIND_EXECUTE, 0, id,
                ZbrtCodec.encodeExecute(argv, cwd, stdin == null ? new byte[0] : stdin, timeoutMs)));
        return new ZbrtStreamSession(id);
    }

    /** Reader for one active Execute turn on this connection. */
    public final class ZbrtStreamSession {
        private final byte[] requestId;
        private volatile boolean stopped = false;
        private volatile boolean terminal = false;

        ZbrtStreamSession(byte[] requestId) {
            this.requestId = requestId;
        }

        /** Next event frame: Output or terminal Exit; null only if we already terminated. */
        public Event nextEvent() {
            if (terminal) {
                return null;
            }
            ZbrtFrame frame = ZbrtFrame.decode(in);
            requireId(frame, requestId);
            if (frame.kind() == ZbrtFrame.KIND_OUTPUT) {
                ZbrtCodec.Output output = ZbrtCodec.decodeOutput(frame.payload());
                return new Event(output.stream(), output.data(), null);
            }
            if (frame.kind() == ZbrtFrame.KIND_EXIT) {
                ZbrtCodec.Exit exit = ZbrtCodec.decodeExit(frame.payload());
                terminal = true;
                return new Event(-1, new byte[0], exit.code());
            }
            if (frame.kind() == ZbrtFrame.KIND_ERROR) {
                throw errorFrame(frame);
            }
            throw unexpected(frame, "Output/Exit");
        }

        /** Idempotent stop: Cancel targeting this request, expecting CancelAck. */
        public void stop() {
            if (terminal || stopped) {
                return;
            }
            stopped = true;
            // sendCancel writes the targeted Cancel and consumes the CancelAck.
            // The guest then delivers this turn's single terminal (Exit) which
            // nextEvent() reports to the caller.
            sendCancel(null, requestId);
        }

        public byte[] requestId() {
            return requestId;
        }
    }

    /** One ZbrtStreamSession event: stream 0=stdout 1=stderr, or code != null → Exit. */
    public static final class Event {
        private final int stream;
        private final byte[] data;
        private final Integer code;

        public Event(int stream, byte[] data, Integer code) {
            this.stream = stream;
            this.data = data;
            this.code = code;
        }

        public int stream() { return stream; }
        public byte[] data() { return data; }
        public Integer code() { return code; }

        public boolean isExit() {
            return code != null;
        }

        @Override
        public boolean equals(Object o) {
            if (this == o) return true;
            if (!(o instanceof Event)) return false;
            Event other = (Event) o;
            return stream == other.stream
                    && java.util.Arrays.equals(data, other.data)
                    && java.util.Objects.equals(code, other.code);
        }

        @Override
        public int hashCode() {
            return java.util.Objects.hash(stream, java.util.Arrays.hashCode(data), code);
        }

        @Override
        public String toString() {
            return "Event[stream=" + stream + ", data=" + java.util.Arrays.toString(data)
                    + ", code=" + code + "]";
        }
    }

    public static final class Exec {
        private final int code;
        private final byte[] stdout;
        private final byte[] stderr;
        private final Long signal;
        private final boolean timedOut;

        public Exec(int code, byte[] stdout, byte[] stderr, Long signal, boolean timedOut) {
            this.code = code;
            this.stdout = stdout;
            this.stderr = stderr;
            this.signal = signal;
            this.timedOut = timedOut;
        }

        public int code() { return code; }
        public byte[] stdout() { return stdout; }
        public byte[] stderr() { return stderr; }
        public Long signal() { return signal; }
        public boolean timedOut() { return timedOut; }

        @Override
        public boolean equals(Object o) {
            if (this == o) return true;
            if (!(o instanceof Exec)) return false;
            Exec other = (Exec) o;
            return code == other.code
                    && timedOut == other.timedOut
                    && java.util.Arrays.equals(stdout, other.stdout)
                    && java.util.Arrays.equals(stderr, other.stderr)
                    && java.util.Objects.equals(signal, other.signal);
        }

        @Override
        public int hashCode() {
            return java.util.Objects.hash(code, java.util.Arrays.hashCode(stdout),
                    java.util.Arrays.hashCode(stderr), signal, timedOut);
        }

        @Override
        public String toString() {
            return "Exec[code=" + code + ", stdout=" + java.util.Arrays.toString(stdout)
                    + ", stderr=" + java.util.Arrays.toString(stderr)
                    + ", signal=" + signal + ", timedOut=" + timedOut + "]";
        }
    }

    // ---- plumbing --------------------------------------------------------

    private void sendCancel(String reason, byte[] target16) {
        cancelRoundTrip(reason, target16);
    }

    /**
     * Send Cancel and wait for the CancelAck. The guest may emit straggler
     * Output/Exit frames for the cancelled turn before the ack; they are
     * skipped instead of failing the decode (mirrors the Python reference).
     */
    private ZbrtFrame cancelRoundTrip(String reason, byte[] target16) {
        byte[] id = newRequestId();
        writeFrame(new ZbrtFrame(ZbrtFrame.KIND_CANCEL, 0, id,
                ZbrtCodec.encodeCancel(reason, target16)));
        while (true) {
            ZbrtFrame frame = readReply(id);
            if (frame.kind() == ZbrtFrame.KIND_CANCEL_ACK) {
                return frame;
            }
            if (frame.kind() == ZbrtFrame.KIND_ERROR) {
                throw errorFrame(frame);
            }
            if (frame.kind() == ZbrtFrame.KIND_OUTPUT || frame.kind() == ZbrtFrame.KIND_EXIT) {
                continue;
            }
            throw unexpected(frame, "CancelAck");
        }
    }

    /**
     * Send one request frame and read the reply with a matching request id.
     * An Error reply raises {@link RemoteError}; anything other than
     * {@code ackKind} raises {@link DecodeError}.
     */
    private ZbrtFrame roundTrip(int requestKind, byte[] payload, int ackKind, String expected) {
        byte[] id = newRequestId();
        writeFrame(new ZbrtFrame(requestKind, 0, id, payload));
        ZbrtFrame frame = readReply(id);
        if (frame.kind() == ackKind) {
            return frame;
        }
        if (frame.kind() == ZbrtFrame.KIND_ERROR) {
            throw errorFrame(frame);
        }
        throw unexpected(frame, expected);
    }

    private ZbrtFrame readReply(byte[] requestId) {
        ZbrtFrame frame = ZbrtFrame.decode(in);
        requireId(frame, requestId);
        return frame;
    }

    private void requireId(ZbrtFrame frame, byte[] requestId) {
        if (!java.util.Arrays.equals(frame.requestId(), requestId)) {
            throw new DecodeError("request_id mismatch");
        }
    }

    private RemoteError errorFrame(ZbrtFrame frame) {
        ZbrtCodec.ZbrtErrorPayload error = ZbrtCodec.decodeError(frame.payload());
        return new RemoteError(error.code(), error.message());
    }

    private DecodeError unexpected(ZbrtFrame frame, String expected) {
        return new DecodeError("unexpected frame kind " + frame.kind() + " while awaiting " + expected);
    }

    private void writeFrame(ZbrtFrame frame) {
        frame.writeTo(out);
    }

    @Override
    public synchronized void close() {
        try {
            socket.close();
        } catch (IOException ignored) {
            // best effort
        }
    }
}
