package io.rfb.sdk.internal;

import io.rfb.sdk.DecodeError;

import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;

/**
 * Strict ZBRT v1 payload codecs (PROTOCOL.md §3.2): u32 BE length-prefixed
 * strings/bytes; every decode rejects truncation AND trailing bytes. INTERNAL.
 */
public final class ZbrtCodec {
    private ZbrtCodec() {
    }

    /** Cursor over a payload buffer with strict bounds checking. */
    static final class Reader {
        private final byte[] buf;
        private int pos;

        Reader(byte[] buf) {
            this.buf = buf;
        }

        boolean hasRemaining() {
            return pos < buf.length;
        }

        private void need(int n) {
            if (buf.length - pos < n) {
                throw new DecodeError("truncated payload");
            }
        }

        int u8() {
            need(1);
            return buf[pos++] & 0xFF;
        }

        boolean flag() {
            return u8() != 0;
        }

        int u32() {
            need(4);
            int v = ((buf[pos] & 0xFF) << 24) | ((buf[pos + 1] & 0xFF) << 16)
                    | ((buf[pos + 2] & 0xFF) << 8) | (buf[pos + 3] & 0xFF);
            pos += 4;
            return v;
        }

        int i32() {
            return u32();
        }

        byte[] bytes(int n) {
            need(n);
            byte[] out = new byte[n];
            System.arraycopy(buf, pos, out, 0, n);
            pos += n;
            return out;
        }

        byte[] prefixedBytes() {
            int n = u32();
            if (n < 0) {
                throw new DecodeError("truncated payload");
            }
            return bytes(n);
        }

        String text() {
            return new String(prefixedBytes(), StandardCharsets.UTF_8);
        }
    }

    static void putText(ByteArrayOutputStream out, String s) {
        byte[] b = s.getBytes(StandardCharsets.UTF_8);
        putU32(out, b.length);
        out.write(b, 0, b.length);
    }

    static void putPrefixed(ByteArrayOutputStream out, byte[] b) {
        putU32(out, b.length);
        out.write(b, 0, b.length);
    }

    /** Optional text: presence flag 0/1, then the length-prefixed UTF-8 text when present. */
    static void putFlagText(ByteArrayOutputStream out, String s) {
        if (s != null) {
            out.write(1);
            putText(out, s);
        } else {
            out.write(0);
        }
    }

    static void putU32(ByteArrayOutputStream out, int v) {
        out.write((v >>> 24) & 0xFF);
        out.write((v >>> 16) & 0xFF);
        out.write((v >>> 8) & 0xFF);
        out.write(v & 0xFF);
    }

    private static void checkTrailing(Reader r) {
        if (r.hasRemaining()) {
            throw new DecodeError("trailing payload");
        }
    }

    private static void checkCount(int count) {
        if (count > 255) {
            throw new DecodeError("too many capabilities");
        }
    }

    // ---- Hello / HelloAck ------------------------------------------------

    public static byte[] encodeHello(String client, java.util.List<String> capabilities) {
        return encodeHelloLike(client, capabilities);
    }

    public static byte[] encodeHelloAck(String server, java.util.List<String> capabilities) {
        return encodeHelloLike(server, capabilities);
    }

    private static byte[] encodeHelloLike(String name, java.util.List<String> capabilities) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        putText(out, name);
        checkCount(capabilities.size());
        out.write(capabilities.size());
        for (String cap : capabilities) {
            putText(out, cap);
        }
        return out.toByteArray();
    }

    public static byte[] encodeOutput(int stream, byte[] data) {
        ByteArrayOutputStream out = new ByteArrayOutputStream(8 + data.length);
        out.write(stream);
        putPrefixed(out, data);
        return out.toByteArray();
    }

    public static byte[] encodeError(long code, String message) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        putU32(out, (int) code);
        putText(out, message);
        return out.toByteArray();
    }

    public static HelloAck decodeHelloAck(byte[] payload) {
        Reader r = new Reader(payload);
        String server = r.text();
        int n = r.u8();
        java.util.List<String> capabilities = new java.util.ArrayList<>(n);
        for (int i = 0; i < n; i++) {
            capabilities.add(r.text());
        }
        checkTrailing(r);
        return new HelloAck(server, capabilities);
    }

    // ---- Execute ---------------------------------------------------------

    public static byte[] encodeExecute(java.util.List<String> argv, String cwd,
                                       byte[] stdin, long timeoutMs) {
        if (argv.size() > 255) {
            throw new DecodeError("too many arguments");
        }
        if (timeoutMs < 0 || timeoutMs > 0xFFFFFFFFL) {
            throw new DecodeError("timeout_ms out of u32 range");
        }
        ByteArrayOutputStream out = new ByteArrayOutputStream(48 + stdin.length);
        out.write(argv.size());
        for (String arg : argv) {
            putText(out, arg);
        }
        putFlagText(out, cwd);
        putPrefixed(out, stdin);
        putU32(out, (int) timeoutMs);
        return out.toByteArray();
    }

    // ---- Output ----------------------------------------------------------

    public static Output decodeOutput(byte[] payload) {
        Reader r = new Reader(payload);
        int stream = r.u8();
        byte[] data = r.prefixedBytes();
        checkTrailing(r);
        return new Output(stream, data);
    }

    public static Execute decodeExecute(byte[] payload) {
        Reader r = new Reader(payload);
        int argc = r.u8();
        java.util.List<String> argv = new java.util.ArrayList<>(argc);
        for (int i = 0; i < argc; i++) {
            argv.add(r.text());
        }
        String cwd = r.flag() ? r.text() : null;
        byte[] stdin = r.prefixedBytes();
        long timeoutMs = r.u32() & 0xFFFFFFFFL;
        checkTrailing(r);
        return new Execute(argv, cwd, stdin, timeoutMs);
    }

    // ---- Exit ------------------------------------------------------------

    public static byte[] encodeExit(int code, Long signal) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        putU32(out, code);
        if (signal != null) {
            out.write(1);
            putU32(out, signal.intValue());
        } else {
            out.write(0);
        }
        return out.toByteArray();
    }

    public static Exit decodeExit(byte[] payload) {
        Reader r = new Reader(payload);
        int code = r.i32();
        Long signal = null;
        if (r.flag()) {
            signal = (long) r.u32() & 0xFFFFFFFFL;
        }
        checkTrailing(r);
        return new Exit(code, signal);
    }

    // ---- Cancel ----------------------------------------------------------

    public static byte[] encodeCancel(String reason, byte[] target16) {
        ByteArrayOutputStream out = new ByteArrayOutputStream(24);
        putFlagText(out, reason);
        if (target16 != null) {
            if (target16.length != 16) {
                throw new DecodeError("cancel target must be 16 bytes");
            }
            out.write(1);
            out.write(target16, 0, target16.length);
        } else {
            out.write(0);
        }
        return out.toByteArray();
    }

    public static Cancel decodeCancel(byte[] payload) {
        Reader r = new Reader(payload);
        String reason = r.flag() ? r.text() : null;
        byte[] target;
        if (!r.hasRemaining()) {
            target = null; // legacy payload without the target byte
        } else if (r.flag()) {
            target = r.bytes(16);
        } else {
            target = null;
        }
        checkTrailing(r);
        return new Cancel(reason, target);
    }

    // ---- Fs --------------------------------------------------------------

    public static byte[] encodeFs(int op, String path, byte[] data) {
        ByteArrayOutputStream out = new ByteArrayOutputStream(24 + data.length);
        out.write(op);
        putText(out, path);
        putPrefixed(out, data);
        return out.toByteArray();
    }

    public static Fs decodeFs(byte[] payload) {
        Reader r = new Reader(payload);
        int op = r.u8();
        String path = r.text();
        byte[] data = r.prefixedBytes();
        checkTrailing(r);
        return new Fs(op, path, data);
    }

    // ---- Health / HealthAck ----------------------------------------------

    public static byte[] encodeHealth(boolean healthy, String message) {
        ByteArrayOutputStream out = new ByteArrayOutputStream(16);
        out.write(healthy ? 1 : 0);
        putFlagText(out, message);
        return out.toByteArray();
    }

    public static Health decodeHealth(byte[] payload) {
        Reader r = new Reader(payload);
        boolean healthy = r.flag();
        String message = r.flag() ? r.text() : null;
        checkTrailing(r);
        return new Health(healthy, message);
    }

    // ---- Error -----------------------------------------------------------

    public static ZbrtErrorPayload decodeError(byte[] payload) {
        Reader r = new Reader(payload);
        long code = r.u32() & 0xFFFFFFFFL;
        String message = r.text();
        checkTrailing(r);
        return new ZbrtErrorPayload(code, message);
    }

    // ---- value holders -----------------------------------------------------
    // Java-8 compatible value holders: originally records, converted so the
    // artifact targets Java 8. Accessor names match the record components.

    /** HelloAck value: server name + negotiated capabilities. */
    public static final class HelloAck {
        public final String server;
        public final java.util.List<String> capabilities;

        public HelloAck(String server, java.util.List<String> capabilities) {
            this.server = server;
            this.capabilities = capabilities;
        }
    }

    public static final class Output {
        private final int stream;
        private final byte[] data;

        public Output(int stream, byte[] data) {
            this.stream = stream;
            this.data = data;
        }

        public int stream() { return stream; }
        public byte[] data() { return data; }

        @Override
        public boolean equals(Object o) {
            if (this == o) return true;
            if (!(o instanceof Output)) return false;
            Output other = (Output) o;
            return stream == other.stream && java.util.Arrays.equals(data, other.data);
        }

        @Override
        public int hashCode() {
            return java.util.Objects.hash(stream, java.util.Arrays.hashCode(data));
        }

        @Override
        public String toString() {
            return "Output[stream=" + stream + ", data=" + java.util.Arrays.toString(data) + "]";
        }
    }

    public static final class Execute {
        private final java.util.List<String> argv;
        private final String cwd;
        private final byte[] stdin;
        private final long timeoutMs;

        public Execute(java.util.List<String> argv, String cwd, byte[] stdin, long timeoutMs) {
            this.argv = argv;
            this.cwd = cwd;
            this.stdin = stdin;
            this.timeoutMs = timeoutMs;
        }

        public java.util.List<String> argv() { return argv; }
        public String cwd() { return cwd; }
        public byte[] stdin() { return stdin; }
        public long timeoutMs() { return timeoutMs; }

        @Override
        public boolean equals(Object o) {
            if (this == o) return true;
            if (!(o instanceof Execute)) return false;
            Execute other = (Execute) o;
            return timeoutMs == other.timeoutMs
                    && java.util.Objects.equals(argv, other.argv)
                    && java.util.Objects.equals(cwd, other.cwd)
                    && java.util.Arrays.equals(stdin, other.stdin);
        }

        @Override
        public int hashCode() {
            return java.util.Objects.hash(argv, cwd, java.util.Arrays.hashCode(stdin), timeoutMs);
        }

        @Override
        public String toString() {
            return "Execute[argv=" + argv + ", cwd=" + cwd + ", stdin="
                    + java.util.Arrays.toString(stdin) + ", timeoutMs=" + timeoutMs + "]";
        }
    }

    public static final class Exit {
        private final int code;
        private final Long signal;

        public Exit(int code, Long signal) {
            this.code = code;
            this.signal = signal;
        }

        public int code() { return code; }
        public Long signal() { return signal; }

        @Override
        public boolean equals(Object o) {
            if (this == o) return true;
            if (!(o instanceof Exit)) return false;
            Exit other = (Exit) o;
            return code == other.code && java.util.Objects.equals(signal, other.signal);
        }

        @Override
        public int hashCode() {
            return java.util.Objects.hash(code, signal);
        }

        @Override
        public String toString() {
            return "Exit[code=" + code + ", signal=" + signal + "]";
        }
    }

    public static final class Cancel {
        private final String reason;
        private final byte[] target;

        public Cancel(String reason, byte[] target) {
            this.reason = reason;
            this.target = target;
        }

        public String reason() { return reason; }
        public byte[] target() { return target; }

        @Override
        public boolean equals(Object o) {
            if (this == o) return true;
            if (!(o instanceof Cancel)) return false;
            Cancel other = (Cancel) o;
            return java.util.Objects.equals(reason, other.reason)
                    && java.util.Arrays.equals(target, other.target);
        }

        @Override
        public int hashCode() {
            return java.util.Objects.hash(reason, java.util.Arrays.hashCode(target));
        }

        @Override
        public String toString() {
            return "Cancel[reason=" + reason + ", target=" + java.util.Arrays.toString(target) + "]";
        }
    }

    public static final class Fs {
        private final int op;
        private final String path;
        private final byte[] data;

        public Fs(int op, String path, byte[] data) {
            this.op = op;
            this.path = path;
            this.data = data;
        }

        public int op() { return op; }
        public String path() { return path; }
        public byte[] data() { return data; }

        @Override
        public boolean equals(Object o) {
            if (this == o) return true;
            if (!(o instanceof Fs)) return false;
            Fs other = (Fs) o;
            return op == other.op
                    && java.util.Objects.equals(path, other.path)
                    && java.util.Arrays.equals(data, other.data);
        }

        @Override
        public int hashCode() {
            return java.util.Objects.hash(op, path, java.util.Arrays.hashCode(data));
        }

        @Override
        public String toString() {
            return "Fs[op=" + op + ", path=" + path + ", data=" + java.util.Arrays.toString(data) + "]";
        }
    }

    public static final class Health {
        private final boolean healthy;
        private final String message;

        public Health(boolean healthy, String message) {
            this.healthy = healthy;
            this.message = message;
        }

        public boolean healthy() { return healthy; }
        public String message() { return message; }

        @Override
        public boolean equals(Object o) {
            if (this == o) return true;
            if (!(o instanceof Health)) return false;
            Health other = (Health) o;
            return healthy == other.healthy && java.util.Objects.equals(message, other.message);
        }

        @Override
        public int hashCode() {
            return java.util.Objects.hash(healthy, message);
        }

        @Override
        public String toString() {
            return "Health[healthy=" + healthy + ", message=" + message + "]";
        }
    }

    public static final class ZbrtErrorPayload {
        private final long code;
        private final String message;

        public ZbrtErrorPayload(long code, String message) {
            this.code = code;
            this.message = message;
        }

        public long code() { return code; }
        public String message() { return message; }

        @Override
        public boolean equals(Object o) {
            if (this == o) return true;
            if (!(o instanceof ZbrtErrorPayload)) return false;
            ZbrtErrorPayload other = (ZbrtErrorPayload) o;
            return code == other.code && java.util.Objects.equals(message, other.message);
        }

        @Override
        public int hashCode() {
            return java.util.Objects.hash(code, message);
        }

        @Override
        public String toString() {
            return "ZbrtErrorPayload[code=" + code + ", message=" + message + "]";
        }
    }
}
