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
     * Read the next event. Returns null on clean close. The terminal
     * {@code exit} event (with code, possibly null) is returned normally.
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
     * Send input to the running stream. Only supported over the NDJSON
     * transport; over ZBRT v1 raises {@link RemoteError}. Raises
     * {@link RemoteError} after the stream terminated or was stopped.
     */
    public void sendInput(String text) {
        if (ndjson != null) {
            ndjson.sendInput(text);
            return;
        }
        throw new RemoteError("ZeroBoot V1 streams do not support input");
    }

    /** Request termination; idempotent and safe after the exit event. */
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
        JsonNode code = value.get("exit_code");
        if (code != null && code.isNumber()) {
            return StreamEvent.exit(code.intValue());
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
