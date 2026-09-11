package io.rfb.sdk.internal;

import com.fasterxml.jackson.databind.JsonNode;
import io.rfb.sdk.RemoteError;

import java.io.BufferedInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.time.Duration;
import java.util.List;

/**
 * One bidirectional NDJSON guest stream over a single TCP connection
 * (PROTOCOL.md §2.2 "stream"). Yields raw JSON event lines; the public facade
 * maps them to {@code io.rfb.sdk.StreamEvent}. INTERNAL.
 */
public final class GuestNdjsonStream {
    private final Socket socket;
    private final InputStream in;
    private final OutputStream out;
    // volatile: close() is synchronized but nextEvent/sendInput/stop are not;
    // the flags are the only cross-thread state a caller observes.
    private volatile boolean stopped = false;
    private volatile boolean terminal = false;
    private volatile boolean closed = false;

    GuestNdjsonStream(Socket socket) throws IOException {
        this.socket = socket;
        this.in = new BufferedInputStream(socket.getInputStream());
        this.out = socket.getOutputStream();
    }

    public static GuestNdjsonStream open(InetSocketAddress address, Duration timeout,
                                         JsonNode action) {
        Socket socket = GuestNdjson.connect(address, timeout);
        try {
            GuestNdjson.writeLine(socket.getOutputStream(), action);
            return new GuestNdjsonStream(socket);
        } catch (IOException e) {
            try {
                socket.close();
            } catch (IOException ignored) {
                // best effort
            }
            throw new io.rfb.sdk.TransportError("guest stream handshake failed: " + e.getMessage(), e);
        }
    }

    /** Next raw event line; null on clean close. Marks the stream terminal on exit_code. */
    public JsonNode nextEvent() {
        JsonNode value = GuestNdjson.readLine(in);
        if (value == null) {
            return null;
        }
        GuestNdjson.checkRemoteError(value);
        if (value.has("exit_code")) {
            terminal = true;
        }
        return value;
    }

    /** Send {@code {"in": text}} to the running stream. */
    public void sendInput(String input) {
        if (terminal || stopped) {
            throw new RemoteError("guest stream is no longer running");
        }
        GuestNdjson.writeLine(out, Json.object().put("in", input));
    }

    /** Send {@code {"action":"stop"}}; idempotent, safe after termination. */
    public void stop() {
        if (terminal || stopped) {
            return;
        }
        stopped = true;
        GuestNdjson.writeLine(out, Json.object().put("action", "stop"));
    }

    public boolean isTerminal() {
        return terminal;
    }

    public synchronized void close() {
        if (closed) {
            return;
        }
        closed = true;
        try {
            socket.close();
        } catch (IOException ignored) {
            // best effort
        }
    }

    /** Convenience: read events until the terminal {@code exit_code} event or clean close. */
    public List<JsonNode> drainToTerminal() {
        List<JsonNode> events = new java.util.ArrayList<>();
        while (!terminal) {
            JsonNode event = nextEvent();
            if (event == null) {
                return events;
            }
            events.add(event);
        }
        return events;
    }
}
