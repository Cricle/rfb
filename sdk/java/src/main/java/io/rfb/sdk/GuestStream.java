package io.rfb.sdk;

import com.fasterxml.jackson.databind.JsonNode;
import io.rfb.sdk.internal.GuestNdjsonStream;
import io.rfb.sdk.internal.Json;
import io.rfb.sdk.internal.ZbrtConnection;

/**
 * Interactive guest stream (UNIFIED_API.md §5): read events with
 * {@link #nextEvent()} until the terminal {@code exit} event, send input with
 * {@link #sendInput(String)} (NDJSON transport only) and request termination
 * with the idempotent {@link #stop()}.
 */
public final class GuestStream implements AutoCloseable {
    private final GuestNdjsonStream ndjson;
    private final ZbrtConnection zbrtConnection;
    private final ZbrtConnection.ZbrtStreamSession zbrtSession;

    private GuestStream(GuestNdjsonStream ndjson) {
        this.ndjson = ndjson;
        this.zbrtConnection = null;
        this.zbrtSession = null;
    }

    private GuestStream(ZbrtConnection connection, ZbrtConnection.ZbrtStreamSession session) {
        this.ndjson = null;
        this.zbrtConnection = connection;
        this.zbrtSession = session;
    }

    static GuestStream overNdjson(GuestNdjsonStream stream) {
        return new GuestStream(stream);
    }

    static GuestStream overZbrt(ZbrtConnection connection, ZbrtConnection.ZbrtStreamSession session) {
        return new GuestStream(connection, session);
    }

    /**
     * Read the next event. Returns {@code null} once the stream closed
     * cleanly (after the terminal {@code exit} event was delivered, or when
     * the peer disconnected). The terminal {@code exit} event itself (with
     * code, possibly null) is returned normally.
     *
     * @return the next event, or {@code null} after clean close
     * @throws TransportError read failure or timeout
     * @throws RemoteError    the guest sent an {@code error} line/frame
     * @throws DecodeError    the guest sent an undecodable line/frame
     */
    public StreamEvent nextEvent() {
        if (ndjson != null) {
            JsonNode node = ndjson.nextEvent();
            return node == null ? null : mapNdjsonEvent(node);
        }
        ZbrtConnection.Event event = zbrtSession.nextEvent();
        if (event == null) {
            return null;
        }
        if (event.isExit()) {
            return StreamEvent.exit(event.code());
        }
        return event.stream() == 0
                ? StreamEvent.stdout(event.data())
                : StreamEvent.stderr(event.data());
    }

    /**
     * Send input to the running stream ({@code {"in": text}} on the wire).
     * Only supported over the NDJSON transport; over ZBRT v1 there is no
     * input channel, so the call raises {@link RemoteError}. Also raises
     * {@link RemoteError} after the stream terminated or was stopped.
     *
     * @param text input bytes (UTF-8) to forward to the stream
     * @throws RemoteError ZBRT transport, or stream no longer running
     * @throws TransportError write failure
     */
    public void sendInput(String text) {
        if (ndjson != null) {
            ndjson.sendInput(text);
            return;
        }
        throw new RemoteError("ZeroBoot V1 streams do not support input");
    }

    /**
     * Request termination; idempotent and safe after the exit event. NDJSON
     * sends {@code {"action":"stop"}}; ZBRT sends a Cancel frame targeting
     * this request's id and waits for the (empty) CancelAck, buffering any
     * Output frames that arrive meanwhile (UNIFIED_API.md §5).
     *
     * @throws TransportError write/read failure while cancelling
     * @throws RemoteError    the guest answered the cancel with an Error frame
     */
    public void stop() {
        if (ndjson != null) {
            ndjson.stop();
            return;
        }
        zbrtSession.stop();
    }

    @Override
    public void close() {
        if (ndjson != null) {
            ndjson.close();
            return;
        }
        zbrtConnection.close();
    }

    /**
     * NDJSON event mapping (mirrors the Rust {@code forkd_stream_event}):
     * started → stdout → stderr → exit ordering per detection keys.
     */
    static StreamEvent mapNdjsonEvent(JsonNode value) {
        if (isStarted(value)) {
            return StreamEvent.started();
        }
        // A terminal exit line is any line CARRYING an `exit_code` key — the
        // guest emits `{"exit_code":null}` when the child died by signal, and
        // that is a terminal Exit(null), not a protocol error.
        if (value.has("exit_code")) {
            JsonNode code = value.get("exit_code");
            return StreamEvent.exit(code != null && code.isIntegralNumber() ? code.intValue() : null);
        }
        if (value.path("done").asBoolean(false)) {
            return StreamEvent.exit(null);
        }
        if (value.has("stdout") || value.has("out")) {
            return StreamEvent.stdout(Json.valueBytes(firstOf(value, "stdout", "out")));
        }
        if (value.has("stderr") || value.has("err")) {
            return StreamEvent.stderr(Json.valueBytes(firstOf(value, "stderr", "err")));
        }
        throw new DecodeError("invalid guest stream event");
    }

    private static boolean isStarted(JsonNode value) {
        return value.path("started").asBoolean(false)
                || "started".equals(value.path("stream").asText(null))
                || "started".equals(value.path("event").asText(null));
    }

    private static JsonNode firstOf(JsonNode value, String a, String b) {
        JsonNode first = value.get(a);
        return first != null ? first : value.get(b);
    }
}
