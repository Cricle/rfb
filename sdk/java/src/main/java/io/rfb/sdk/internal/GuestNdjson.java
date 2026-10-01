package io.rfb.sdk.internal;

import com.fasterxml.jackson.databind.JsonNode;
import io.rfb.sdk.DecodeError;
import io.rfb.sdk.RemoteError;
import io.rfb.sdk.RfbError;
import io.rfb.sdk.TransportError;

import java.io.BufferedInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;

/**
 * forkd guest TCP NDJSON transport (PROTOCOL.md §2). Each request opens one
 * connection, sends one JSON line and reads response lines until a terminal
 * line. INTERNAL — the public surface is {@code io.rfb.sdk.Sandbox}.
 */
public final class GuestNdjson {
    /**
     * Agent-token environment variable (cross-language contract §8): when set
     * to a non-blank value, every guest NDJSON connection must authenticate
     * before any request line is sent. Unset/blank = unchanged wire behavior.
     */
    public static final String AGENT_TOKEN_ENV = "FORKD_AGENT_TOKEN";

    /**
     * Test seam only: the JVM cannot mutate its own environment, so tests
     * inject the token here. Non-null wins over the environment; production
     * behavior reads {@link #AGENT_TOKEN_ENV} exclusively.
     */
    public static volatile String agentTokenOverride;

    private GuestNdjson() {
    }

    /** Configured agent token, or null when auth is disabled (blank = unset). */
    static String agentToken() {
        String token = agentTokenOverride != null
                ? agentTokenOverride : System.getenv(AGENT_TOKEN_ENV);
        return token == null || token.trim().isEmpty() ? null : token;
    }

    /**
     * Agent-token handshake on a fresh guest connection: with a token
     * configured the FIRST line is {@code {"action":"auth","token":…}} and the
     * agent must answer {@code {"action":"auth","ok":true}}; any other reply
     * (including an {@code error} line) is a Remote-class failure. Without a
     * token nothing is written (mirrors Python {@code _guest.py}).
     */
    public static void authenticate(InputStream in, OutputStream out) {
        String token = agentToken();
        if (token == null) {
            return;
        }
        writeLine(out, Json.object().put("action", "auth").put("token", token));
        JsonNode value = readLine(in);
        if (value == null) {
            throw new RemoteError("guest closed before response");
        }
        checkRemoteError(value);
        if (!("auth".equals(value.path("action").asText()) && value.path("ok").asBoolean(false))) {
            throw new RemoteError("guest agent auth failed");
        }
    }

    public static InetSocketAddress parseAddress(String address) {
        if (address == null || address.isEmpty()) {
            throw new io.rfb.sdk.ValidationError("guest address must be host:port");
        }
        int idx = address.lastIndexOf(':');
        if (idx < 0) {
            throw new io.rfb.sdk.ValidationError("invalid guest address: expected host:port");
        }
        String host = address.substring(0, idx);
        int port;
        try {
            port = Integer.parseInt(address.substring(idx + 1));
        } catch (NumberFormatException e) {
            throw new io.rfb.sdk.ValidationError("invalid guest address port");
        }
        if (host.startsWith("[") && host.endsWith("]")) {
            host = host.substring(1, host.length() - 1);
        }
        if (host.isEmpty() || port < 1 || port > 65535) {
            throw new io.rfb.sdk.ValidationError("invalid guest address: expected host:port");
        }
        return InetSocketAddress.createUnresolved(host, port);
    }

    /** Open a connected socket with connect/read timeouts applied. */
    public static Socket connect(InetSocketAddress address, Duration timeout) {
        if (address.isUnresolved()) {
            // Resolve explicitly before connecting: the JDK's unresolved-address
            // connect path re-resolves via getaddrinfo, which fails even for
            // literal IPv4 in some minimal environments.
            try {
                address = new InetSocketAddress(
                        java.net.InetAddress.getByName(address.getHostString()), address.getPort());
            } catch (IOException e) {
                throw new TransportError("guest connect failed: " + address.getHostString(), e);
            }
        }
        Socket socket = new Socket();
        try {
            int timeoutMs = (int) Math.min(Integer.MAX_VALUE, timeout.toMillis());
            socket.connect(address, timeoutMs);
            socket.setSoTimeout(timeoutMs);
            socket.setTcpNoDelay(true);
            return socket;
        } catch (IOException e) {
            try {
                socket.close();
            } catch (IOException ignored) {
                // best effort
            }
            throw new TransportError("guest connect failed: " + e.getMessage(), e);
        }
    }

    /** Send one action line and collect JSON response lines until a terminal line. */
    public static List<JsonNode> request(InetSocketAddress address, Duration timeout, JsonNode action) {
        try (Socket socket = connect(address, timeout)) {
            OutputStream out = socket.getOutputStream();
            InputStream in = new BufferedInputStream(socket.getInputStream());
            authenticate(in, out);
            writeLine(out, action);
            List<JsonNode> responses = new ArrayList<>();
            while (true) {
                JsonNode line = readLine(in);
                if (line == null) {
                    throw new RemoteError("guest closed before response");
                }
                checkRemoteError(line);
                responses.add(line);
                if (isTerminal(line)) {
                    return responses;
                }
            }
        } catch (IOException e) {
            throw new TransportError("guest connection failed: " + e.getMessage(), e);
        }
    }

    /** NDJSON 温连接池：agent 的 serve 循环在一条连接上顺序承载多个请求，
     * 每请求新建 TCP 的握手/拆除 ≈ 0.4ms。条目 = socket + 已缓冲的流；
     * 空闲 >1s 的连接直接弃用重连（ndjson 无握手，connect 本身就是验证）；
     * 借出上失败 = 请求可能已在 guest 执行，绝不重试（弃用连接）。 */
    public static final class Pool {
        private final InetSocketAddress address;
        private final Duration timeout;
        private final java.util.concurrent.ConcurrentLinkedQueue<Entry> idle =
                new java.util.concurrent.ConcurrentLinkedQueue<>();

        private static final class Entry {
            final Socket socket;
            final InputStream in;
            final OutputStream out;
            final long lastUsedNanos;

            Entry(Socket socket, InputStream in, OutputStream out, long lastUsedNanos) {
                this.socket = socket;
                this.in = in;
                this.out = out;
                this.lastUsedNanos = lastUsedNanos;
            }
        }

        public Pool(InetSocketAddress address, Duration timeout) {
            this.address = address;
            this.timeout = timeout;
        }

        /** 丢弃所有空闲连接（sandbox 删除/停机）。 */
        public void drain() {
            Entry entry;
            while ((entry = idle.poll()) != null) {
                try {
                    entry.socket.close();
                } catch (IOException ignored) {
                    // best effort
                }
            }
        }

        public List<JsonNode> request(JsonNode action, Duration socketBudget)
                throws IOException {
            Entry entry = idle.poll();
            if (entry != null
                    && System.nanoTime() - entry.lastUsedNanos < 1_000_000_000L) {
                try {
                    // 本操作的读预算刷新 SoTimeout（exec 的宽预算不能被
                    // 建连时的基础超时钉死）。
                    entry.socket.setSoTimeout((int) socketBudget.toMillis());
                    List<JsonNode> responses = exchange(entry.in, entry.out, action);
                    idle.offer(new Entry(entry.socket, entry.in, entry.out,
                            System.nanoTime()));
                    return responses;
                } catch (RfbError | IOException e) {
                    try {
                        entry.socket.close();
                    } catch (IOException ignored) {
                        // best effort
                    }
                    if (e instanceof RfbError) {
                        throw (RfbError) e;
                    }
                    throw new TransportError("guest connection failed: " + e.getMessage(), e);
                }
            }
            if (entry != null) {
                try {
                    entry.socket.close();
                } catch (IOException ignored) {
                    // best effort
                }
            }
            Socket socket = connect(address, timeout);
            try {
                socket.setSoTimeout((int) socketBudget.toMillis());
                OutputStream out = socket.getOutputStream();
                InputStream in = new BufferedInputStream(socket.getInputStream());
                authenticate(in, out);
                List<JsonNode> responses = exchange(in, out, action);
                idle.offer(new Entry(socket, in, out, System.nanoTime()));
                return responses;
            } catch (RfbError | IOException e) {
                try {
                    socket.close();
                } catch (IOException ignored) {
                    // best effort
                }
                if (e instanceof RfbError) {
                    throw (RfbError) e;
                }
                throw new TransportError("guest connection failed: " + e.getMessage(), e);
            }
        }

        private static List<JsonNode> exchange(InputStream in, OutputStream out,
                JsonNode action) throws IOException {
            writeLine(out, action);
            List<JsonNode> responses = new ArrayList<>();
            while (true) {
                JsonNode line = readLine(in);
                if (line == null) {
                    throw new RemoteError("guest closed before response");
                }
                checkRemoteError(line);
                responses.add(line);
                if (isTerminal(line)) {
                    return responses;
                }
            }
        }
    }

    public static void writeLine(OutputStream out, JsonNode node) {
        byte[] line = Json.write(node);
        // One write for line + newline: with TCP_NODELAY two writes are two
        // small segments; coalescing keeps every request line in one segment.
        byte[] buf = new byte[line.length + 1];
        System.arraycopy(line, 0, buf, 0, line.length);
        buf[line.length] = '\n';
        try {
            out.write(buf);
            out.flush();
        } catch (IOException e) {
            throw new TransportError("guest write failed: " + e.getMessage(), e);
        }
    }

    /**
     * Read one NDJSON line (max 1 MiB). Returns null on clean close before any
     * byte; skips empty lines; strips trailing CR/LF.
     */
    public static JsonNode readLine(InputStream in) {
        while (true) {
            // Typical guest JSON responses are hundreds of bytes; pre-sizing
            // avoids the default 32-byte buffer's early doubling copies.
            ByteArrayOutputStream buf = new ByteArrayOutputStream(512);
            try {
                int b;
                while ((b = in.read()) != '\n') {
                    if (b < 0) {
                        if (buf.size() == 0) {
                            return null; // clean close
                        }
                        throw new DecodeError("guest response exceeded "
                                + Validation.MAX_LINE_BYTES + " bytes (no terminating newline)");
                    }
                    buf.write(b);
                    if (buf.size() > Validation.MAX_LINE_BYTES) {
                        throw new DecodeError("guest response exceeded "
                                + Validation.MAX_LINE_BYTES + " bytes");
                    }
                }
            } catch (IOException e) {
                throw new TransportError("guest read failed: " + e.getMessage(), e);
            }
            byte[] raw = buf.toByteArray();
            int size = raw.length;
            while (size > 0 && (raw[size - 1] == '\n' || raw[size - 1] == '\r')) {
                size--;
            }
            if (size == 0) {
                continue; // skip empty lines
            }
            return Json.parse(raw, 0, size);
        }
    }

    /** Any response line carrying a string {@code "error"} key raises immediately. */
    public static void checkRemoteError(JsonNode value) {
        JsonNode error = value.get("error");
        if (error != null && error.isTextual()) {
            throw new RemoteError(error.asText());
        }
    }

    /** Terminal-line detection: any of the PROTOCOL.md §2.1 terminal keys. */
    public static boolean isTerminal(JsonNode value) {
        return value.has("exit_code") || value.has("pong") || value.has("results")
                || value.has("entries") || value.has("matches") || value.has("data")
                || value.has("content") || value.has("output") || value.has("status")
                || value.has("ok") || value.has("healthy") || value.has("done")
                || value.has("cancelled") || value.has("bytes_written");
    }

    public static JsonNode last(List<JsonNode> responses, String what) {
        if (responses.isEmpty()) {
            throw new RemoteError("empty " + what + " response");
        }
        return responses.get(responses.size() - 1);
    }
}
