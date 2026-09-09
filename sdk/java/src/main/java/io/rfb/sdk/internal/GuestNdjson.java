package io.rfb.sdk.internal;

import com.fasterxml.jackson.databind.JsonNode;
import io.rfb.sdk.DecodeError;
import io.rfb.sdk.RemoteError;
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
    private GuestNdjson() {
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
            socket.connect(address, (int) timeout.toMillis());
            socket.setSoTimeout((int) timeout.toMillis());
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
