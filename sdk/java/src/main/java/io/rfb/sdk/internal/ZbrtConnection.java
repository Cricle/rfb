package io.rfb.sdk.internal;

import io.rfb.sdk.DecodeError;
import io.rfb.sdk.RemoteError;
import io.rfb.sdk.RfbError;
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
import java.util.concurrent.ConcurrentLinkedDeque;

/**
 * One ZBRT v1 session over a TCP socket (PROTOCOL.md §3.4, mirroring
 * {@code zeroboot_connection.rs} semantics): mandatory Hello handshake on
 * connect (client name {@code rfb-sdk-java}), fresh 128-bit request id per
 * request, Output frames strictly before exactly one terminal frame,
 * idempotent Cancel with empty-payload CancelAck, Error frames raise
 * {@link RemoteError}, and a 16 MiB cap on one exec turn's aggregated output.
 * INTERNAL.
 */
public final class ZbrtConnection implements AutoCloseable {
    /** ZBRT V1 wire capability vocabulary (single source of truth in the Rust crate). */
    public static final List<String> V1_CAPABILITIES =
            Collections.unmodifiableList(Arrays.asList(
                    "execute", "stream", "deadline", "health", "cancel", "filesystem"));

    /** Client name sent in the mandatory Hello handshake. */
    public static final String CLIENT_NAME = "rfb-sdk-java";

    /** Shared per-process source: SecureRandom.nextBytes is thread-safe. */
    private static final SecureRandom RANDOM = new SecureRandom();

    private final Socket socket;
    private final InputStream in;
    private final java.io.OutputStream out;

    /** 池借出时刷新读停顿预算（exec 的宽预算不能被上一操作的基础预算
     * 钉死；反之归还后挂住的读也不该阻塞 75s）。NDJSON 池的
     * setSoTimeout 是同一模式。 */
    public void setSocketBudget(java.time.Duration budget) {
        try {
            socket.setSoTimeout((int) budget.toMillis());
        } catch (java.io.IOException e) {
            throw new TransportError("failed to set socket budget: " + e.getMessage(), e);
        }
    }

    public ZbrtConnection(InetSocketAddress address, Duration timeout) {
        this.socket = GuestNdjson.connect(address, timeout);
        try {
            this.in = new BufferedInputStream(socket.getInputStream());
            this.out = socket.getOutputStream();
        } catch (IOException e) {
            closeQuietly();
            throw new TransportError("zbrt stream setup failed: " + e.getMessage(), e);
        }
        handshake();
    }

    /**
     * Mandatory Hello handshake: every connection sends Hello first and must
     * receive a well-formed HelloAck before anything else may be exchanged.
     * Any failure (Error reply, unexpected frame, decode failure, EOF) is
     * transport-class — the guest is unusable before it answers.
     */
    private void handshake() {
        byte[] id = newRequestId();
        try {
            writeFrame(new ZbrtFrame(ZbrtFrame.KIND_HELLO, 0, id,
                    ZbrtCodec.encodeHello(CLIENT_NAME, V1_CAPABILITIES)));
            ZbrtFrame frame = readReply(id);
            if (frame.kind() != ZbrtFrame.KIND_HELLO_ACK) {
                throw new DecodeError("expected HelloAck, got frame kind " + frame.kind());
            }
            ZbrtCodec.decodeHelloAck(frame.payload());
        } catch (RfbError e) {
            closeQuietly();
            throw new TransportError("zbrt hello handshake failed: " + e.getMessage(), e);
        }
    }

    /** Fresh 128-bit request id, generated per request. */
    public byte[] newRequestId() {
        byte[] id = new byte[16];
        RANDOM.nextBytes(id);
        return id;
    }

    /** Re-run the Hello handshake under a different client name. */
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
     * one terminal frame — Exit (completed/cancelled) or Error (raise). The
     * turn's aggregated output (stdout+stderr) is capped at 16 MiB; exceeding
     * it raises {@link RemoteError} (mirrors the Rust client's
     * {@code MAX_EXEC_BYTES} guard). {@code timeoutMs} is sent on the wire as
     * the guest-side deadline; a read stall at the connection timeout is a
     * transport-level failure ({@link TransportError}) — the client never
     * cancels on its own (the Rust baseline surfaces timeouts the same way).
     */
    public Exec execute(List<String> argv, String cwd, byte[] stdin, long timeoutMs) {
        byte[] id = newRequestId();
        writeFrame(new ZbrtFrame(ZbrtFrame.KIND_EXECUTE, 0, id,
                ZbrtCodec.encodeExecute(argv, cwd, stdin == null ? new byte[0] : stdin, timeoutMs)));
        ByteArrayOutputStream stdoutBuf = new ByteArrayOutputStream();
        ByteArrayOutputStream stderrBuf = new ByteArrayOutputStream();
        long totalOutput = 0;
        while (true) {
            ZbrtFrame frame = ZbrtFrame.decode(in);
            requireId(frame, id);
            if (frame.kind() == ZbrtFrame.KIND_OUTPUT) {
                ZbrtCodec.Output output = ZbrtCodec.decodeOutput(frame.payload());
                totalOutput += output.data().length;
                if (totalOutput > ZbrtFrame.MAX_PAYLOAD) {
                    throw new RemoteError("guest output exceeded the 16 MiB limit");
                }
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
        private volatile boolean startedSent = false;
        // Straggler Output frames — and an Exit that arrived during the stop
        // drain before the CancelAck — buffered so nextEvent() delivers them
        // instead of losing the turn's terminal (Python stop() semantics).
        private final ConcurrentLinkedDeque<Event> pending = new ConcurrentLinkedDeque<>();

        ZbrtStreamSession(byte[] requestId) {
            this.requestId = requestId;
        }

        /**
         * Next event frame. The first call on a turn synthesizes the
         * {@code started} event (ZBRT v1 has no started frame; mirrors the
         * Rust client). Buffered straggler Output frames — and an Exit cached
         * by {@link #stop()} — are delivered before new frames are read. Null
         * only once the turn terminated and every buffered event was consumed.
         */
        public Event nextEvent() {
            if (!startedSent) {
                startedSent = true;
                return Event.started();
            }
            Event buffered = pending.poll();
            if (buffered != null) {
                return buffered;
            }
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

        /**
         * Idempotent stop: the Cancel frame reuses THIS request's id in the
         * header and targets it in the payload, so the guest's CancelAck (and
         * the cancel-induced terminal) route back to this turn and the guest's
         * target-match check passes (Rust {@code send_cancel_target}). The
         * drain keeps straggler Output frames and caches an Exit that arrives
         * before the CancelAck — the turn's single terminal is never dropped
         * (which would deadlock a later {@link #nextEvent()}).
         */
        public void stop() {
            if (terminal || stopped) {
                return;
            }
            stopped = true;
            writeFrame(new ZbrtFrame(ZbrtFrame.KIND_CANCEL, 0, requestId,
                    ZbrtCodec.encodeCancel("stop", requestId)));
            while (true) {
                ZbrtFrame frame = ZbrtFrame.decode(in);
                requireId(frame, requestId);
                if (frame.kind() == ZbrtFrame.KIND_CANCEL_ACK) {
                    return;
                }
                if (frame.kind() == ZbrtFrame.KIND_OUTPUT) {
                    ZbrtCodec.Output output = ZbrtCodec.decodeOutput(frame.payload());
                    pending.add(new Event(output.stream(), output.data(), null));
                } else if (frame.kind() == ZbrtFrame.KIND_EXIT) {
                    ZbrtCodec.Exit exit = ZbrtCodec.decodeExit(frame.payload());
                    terminal = true;
                    pending.add(new Event(-1, new byte[0], exit.code()));
                    return;
                } else if (frame.kind() == ZbrtFrame.KIND_ERROR) {
                    terminal = true;
                    throw errorFrame(frame);
                } else {
                    throw unexpected(frame, "CancelAck");
                }
            }
        }

        public byte[] requestId() {
            return requestId;
        }
    }

    /** One ZbrtStreamSession event: stream 0=stdout 1=stderr, or code != null → Exit. */
    public static final class Event {
        /** Stream marker of the synthesized session-start event (no wire frame). */
        public static final int STREAM_STARTED = -2;

        private final int stream;
        private final byte[] data;
        private final Integer code;

        public Event(int stream, byte[] data, Integer code) {
            this.stream = stream;
            this.data = data;
            this.code = code;
        }

        /** The synthesized session-start event (ZBRT v1 carries none on the wire). */
        public static Event started() {
            return new Event(STREAM_STARTED, new byte[0], null);
        }

        public int stream() { return stream; }
        public byte[] data() { return data; }
        public Integer code() { return code; }

        public boolean isStarted() {
            return stream == STREAM_STARTED;
        }

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

    /** Best-effort socket close on constructor/handshake failure paths. */
    private void closeQuietly() {
        try {
            socket.close();
        } catch (IOException ignored) {
            // best effort
        }
    }
}
