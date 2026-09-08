package io.rfb.sdk;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.rfb.sdk.internal.GuestNdjson;
import io.rfb.sdk.internal.GuestNdjsonStream;
import io.rfb.sdk.internal.Json;
import io.rfb.sdk.internal.Validation;
import io.rfb.sdk.internal.ZbrtConnection;

import java.net.InetSocketAddress;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

/**
 * Sandbox facade over one live forkd sandbox (UNIFIED_API.md §4). Methods have
 * identical names and result shapes over both guest transports —
 * {@code "ndjson"} (default) and {@code "zbrt"} — mirroring the Rust reference.
 *
 * <p>All PROTOCOL.md §2.3 validation runs locally before anything is sent;
 * failures raise {@link ValidationError}.
 */
public final class Sandbox {
    private final RfbClient client;
    private final SandboxInfo info;
    private final String transport;
    private final InetSocketAddress guestAddress;
    private final Duration timeout;

    private Sandbox(RfbClient client, SandboxInfo info, String transport) {
        this.client = client;
        this.info = info;
        this.transport = transport;
        this.guestAddress = GuestNdjson.parseAddress(info.guestAddr);
        this.timeout = Duration.ofMillis((long) (client.getTimeoutS() * 1000));
    }

    static Sandbox attach(RfbClient client, SandboxInfo info, String transport) {
        return new Sandbox(client, info, transport);
    }

    static Sandbox connectById(RfbClient client, String sandboxId, String transport) {
        if (!RfbClient.TRANSPORT_NDJSON.equals(transport) && !RfbClient.TRANSPORT_ZBRT.equals(transport)) {
            throw new ValidationError("transport must be \"ndjson\" or \"zbrt\"");
        }
        for (SandboxInfo info : client.controller().listSandboxes()) {
            if (info.id.equals(sandboxId)) {
                return new Sandbox(client, info, transport);
            }
        }
        throw new RemoteError("sandbox not found: " + sandboxId);
    }

    // ---- properties ------------------------------------------------------

    public String id() {
        return info.id;
    }

    public String snapshotTag() {
        return info.snapshotTag;
    }

    public String guestAddr() {
        return info.guestAddr;
    }

    public Long createdAtUnix() {
        return info.createdAtUnix;
    }

    public SandboxInfo info() {
        return info;
    }

    /** Guest transport of this facade: {@code "ndjson"} or {@code "zbrt"}. */
    public String transport() {
        return transport;
    }

    // ---- health ----------------------------------------------------------

    /** Guest liveness: true when healthy. */
    public boolean ping() {
        if (RfbClient.TRANSPORT_ZBRT.equals(transport)) {
            try (ZbrtConnection conn = openZbrt()) {
                return conn.health().healthy();
            }
        }
        JsonNode pong = ndjsonRequest(Json.object().put("action", "ping")).get("pong");
        return pong != null && pong.asBoolean(false);
    }

    // ---- exec / eval -----------------------------------------------------

    public ExecResult exec(List<String> args) {
        return exec(args, null);
    }

    public ExecResult exec(List<String> args, String cwd) {
        return exec(args, cwd, 60.0);
    }

    public ExecResult exec(List<String> args, String cwd, double timeoutS) {
        return exec(args, cwd, timeoutS, null);
    }

    /**
     * Execute one command. {@code cwd} null resolves to the guest root
     * ({@code /workspace} over NDJSON; no cwd field over ZBRT). {@code stdin}
     * is delivered over the ZBRT transport; the NDJSON wire contract has no
     * exec stdin channel, so non-empty stdin is silently dropped there
     * (Rust baseline behavior).
     */
    public ExecResult exec(List<String> args, String cwd, double timeoutS, byte[] stdin) {
        if (args == null || args.isEmpty()) {
            throw new ValidationError("args must not be empty");
        }
        if (cwd != null) {
            Validation.filePath(cwd);
        }
        if (!(timeoutS > 0) || Double.isNaN(timeoutS) || Double.isInfinite(timeoutS)) {
            throw new ValidationError("exec timeout must be a positive, finite number of seconds");
        }
        if (RfbClient.TRANSPORT_ZBRT.equals(transport)) {
            long timeoutMs = (long) (timeoutS * 1000);
            try (ZbrtConnection conn = openZbrt()) {
                ZbrtConnection.Exec exec = conn.execute(args, cwd, stdin, timeoutMs);
                return new ExecResult(exec.code(), exec.stdout(), exec.stderr(), exec.timedOut());
            }
        }
        // Rust baseline: the NDJSON exec wire contract has no stdin channel;
        // non-empty stdin is silently dropped (delivered only over ZBRT).
        ObjectNode action = Json.object()
                .put("action", "exec")
                .put("cwd", cwd != null ? cwd : "/workspace")
                .put("timeout", timeoutSeconds(timeoutS));
        com.fasterxml.jackson.databind.node.ArrayNode argv = action.withArray("args");
        for (String arg : args) {
            argv.add(arg);
        }
        JsonNode v = ndjsonRequest(action);
        return new ExecResult(
                statusCode(v, -1),
                Json.valueBytes(firstOf(v, "out", "stdout")),
                Json.valueBytes(firstOf(v, "err", "stderr")),
                v.path("timed_out").asBoolean(false));
    }

    public ExecResult eval(String code) {
        return eval(code, null);
    }

    public ExecResult eval(String code, String cwd) {
        return eval(code, cwd, null);
    }

    /**
     * Evaluate a code snippet in the guest. Eval output maps to
     * {@link ExecResult#stdout} on both transports. Over ZBRT (which has no
     * eval opcode) the facade convention is one Execute turn with
     * {@code argv=["eval", code]}, empty stdin and {@code timeout_ms} =
     * whole seconds &times; 1000 (0 when {@code timeoutS} is null) — see
     * {@code sdk/shared/README.md}.
     */
    public ExecResult eval(String code, String cwd, Double timeoutS) {
        Validation.evalCode(code);
        if (cwd != null) {
            Validation.filePath(cwd);
        }
        long timeoutSecs = -1;
        if (timeoutS != null) {
            if (!(timeoutS > 0)) {
                throw new ValidationError("eval timeout must be > 0 seconds");
            }
            timeoutSecs = timeoutSeconds(timeoutS);
            Validation.evalTimeoutSeconds(timeoutSecs);
        }
        if (RfbClient.TRANSPORT_ZBRT.equals(transport)) {
            // Whole seconds as milliseconds; beyond the u32 wire range the
            // deadline is dropped (mirrors the Rust baseline's unwrap_or(0)).
            long timeoutMs = timeoutSecs > 0 && timeoutSecs <= 0xFFFFFFFFL / 1000
                    ? timeoutSecs * 1000
                    : 0;
            try (ZbrtConnection conn = openZbrt()) {
                ZbrtConnection.Exec exec = conn.execute(
                        List.of("eval", code), cwd, new byte[0], timeoutMs);
                return new ExecResult(exec.code(), exec.stdout(), exec.stderr(), exec.timedOut());
            }
        }
        ObjectNode action = Json.object().put("action", "eval").put("code", code);
        if (cwd != null) {
            action.put("cwd", cwd);
        }
        if (timeoutSecs > 0) {
            action.put("timeout", timeoutSecs);
        }
        JsonNode v = ndjsonRequest(action);
        return new ExecResult(
                statusCode(v, 0),
                Json.valueBytes(firstOf(v, "out", "output")),
                new byte[0],
                v.path("timed_out").asBoolean(false));
    }

    // ---- filesystem ------------------------------------------------------

    /** List directory entries (default path "."). */
    public List<DirEntry> ls() {
        return ls(".");
    }

    public List<DirEntry> ls(String path) {
        Validation.fsPath(path);
        JsonNode node = toolOrFs(1, path, Json.object().put("max_results", Validation.MAX_GUEST_RESULTS));
        List<DirEntry> entries = new ArrayList<>();
        for (JsonNode entry : node.path("entries")) {
            entries.add(new DirEntry(
                    entry.path("name").asText(""),
                    entry.path("is_dir").asBoolean(false),
                    entry.hasNonNull("size") ? entry.get("size").asLong() : null));
        }
        return entries;
    }

    /** Find files by name pattern (default path "."). */
    public List<String> find(String pattern) {
        return find(".", pattern);
    }

    public List<String> find(String path, String pattern) {
        Validation.fsPath(path);
        Validation.pattern(pattern);
        JsonNode node = toolOrFs(2, path, Json.object()
                .put("max_results", Validation.MAX_GUEST_RESULTS)
                .put("pattern", pattern));
        List<String> matches = new ArrayList<>();
        for (JsonNode match : node.path("matches")) {
            matches.add(match.asText());
        }
        return matches;
    }

    /** Grep file contents (default path "."). */
    public List<GrepMatch> grep(String pattern) {
        return grep(".", pattern);
    }

    public List<GrepMatch> grep(String path, String pattern) {
        Validation.fsPath(path);
        Validation.pattern(pattern);
        JsonNode node = toolOrFs(3, path, Json.object()
                .put("max_results", Validation.MAX_GUEST_RESULTS)
                .put("pattern", pattern)
                .put("max_bytes", Validation.MAX_GUEST_RESULT_BYTES));
        List<GrepMatch> matches = new ArrayList<>();
        for (JsonNode match : node.path("matches")) {
            matches.add(new GrepMatch(
                    match.path("path").asText(""),
                    match.hasNonNull("line") ? match.get("line").asLong() : null,
                    match.hasNonNull("column") ? match.get("column").asLong() : null,
                    match.path("text").asText("")));
        }
        return matches;
    }

    /** Read a guest file (backend byte cap applies). */
    public FileRead read(String path) {
        return read(path, null, null);
    }

    public FileRead read(String path, Long offset, Integer maxBytes) {
        Validation.filePath(path);
        if (maxBytes != null) {
            Validation.limit(maxBytes, Validation.MAX_GUEST_RESULT_BYTES);
        }
        if (RfbClient.TRANSPORT_ZBRT.equals(transport)) {
            // PROTOCOL.md §3.3: the Fs data object always carries the keys,
            // with null for absent optionals ({"offset":..,"max_bytes":..}).
            ObjectNode args = Json.object();
            args.put("offset", offset);
            args.put("max_bytes", maxBytes);
            return fileReadFrom(zbrtFs(4, path, args));
        }
        ObjectNode args = Json.object();
        if (offset != null) {
            args.put("offset", offset);
        }
        if (maxBytes != null) {
            args.put("max_bytes", maxBytes);
        }
        return fileReadFrom(toolOrFs(4, path, args));
    }

    private static FileRead fileReadFrom(JsonNode node) {
        return new FileRead(
                Json.valueBytes(node.get("data")),
                node.path("truncated").asBoolean(false),
                node.hasNonNull("total_bytes") ? node.get("total_bytes").asLong() : null);
    }

    /** Write (replace) a guest file; returns bytes_written. */
    public int write(String path, byte[] data) {
        return write(path, data, false, null);
    }

    /** Write or append to a guest file; returns bytes_written. */
    public int write(String path, byte[] data, boolean append, Integer mode) {
        Validation.filePath(path);
        byte[] bytes = data == null ? new byte[0] : data;
        Validation.payloadSize(bytes.length, Validation.MAX_GUEST_RESULT_BYTES);
        if (RfbClient.TRANSPORT_ZBRT.equals(transport)) {
            // PROTOCOL.md §3.3: {"data":[..],"append":..,"mode":..} — the keys
            // are always present, with null for an absent mode.
            ObjectNode args = Json.object();
            args.set("data", Json.bytesNode(bytes));
            args.put("append", append);
            args.put("mode", mode);
            return zbrtFs(5, path, args).path("bytes_written").asInt(0);
        }
        ObjectNode args = Json.object()
                .put("append", append);
        args.set("data", Json.bytesNode(bytes));
        if (mode != null) {
            args.put("mode", mode);
        }
        ObjectNode action = Json.object()
                .put("action", "write")
                .put("path", path);
        action.setAll(args);
        return (int) toolRequest(action).path("bytes_written").asLong(0);
    }

    // ---- stream ----------------------------------------------------------

    public GuestStream stream(List<String> args) {
        return stream(args, null, null, null);
    }

    /**
     * Open an interactive stream. {@code cwd} is an opaque guest path; env is
     * only applied over the NDJSON transport (ZBRT v1 has no env channel).
     */
    public GuestStream stream(List<String> args, String cwd, Boolean pty, Map<String, String> env) {
        if (cwd != null) {
            Validation.filePath(cwd);
        }
        if (RfbClient.TRANSPORT_ZBRT.equals(transport)) {
            ZbrtConnection conn = openZbrt();
            try {
                return GuestStream.overZbrt(conn, conn.openStreamSession(args, cwd, new byte[0], 0));
            } catch (RuntimeException e) {
                conn.close();
                throw e;
            }
        }
        ObjectNode action = Json.object().put("action", "stream");
        com.fasterxml.jackson.databind.node.ArrayNode argv = action.withArray("args");
        for (String arg : args) {
            argv.add(arg);
        }
        if (cwd != null) {
            action.put("cwd", cwd);
        }
        if (pty != null) {
            action.put("pty", pty);
        }
        if (env != null && !env.isEmpty()) {
            ObjectNode envNode = action.putObject("env");
            for (Map.Entry<String, String> entry : env.entrySet()) {
                envNode.put(entry.getKey(), entry.getValue());
            }
        }
        return GuestStream.overNdjson(GuestNdjsonStream.open(guestAddress, timeout, action));
    }

    // ---- lifecycle -------------------------------------------------------

    /** Delete this sandbox; both 2xx and 404 are success. */
    public void delete() {
        client.deleteSandbox(info.id);
    }

    // ---- plumbing --------------------------------------------------------

    private JsonNode toolOrFs(int op, String path, ObjectNode args) {
        if (RfbClient.TRANSPORT_ZBRT.equals(transport)) {
            return zbrtFs(op, path, args);
        }
        ObjectNode action = Json.object()
                .put("action", actionName(op))
                .put("path", path);
        action.setAll(args);
        return toolRequest(action);
    }

    private static String actionName(int op) {
        switch (op) {
            case 1: return "ls";
            case 2: return "find";
            case 3: return "grep";
            case 4: return "read";
            case 5: return "write";
            default: throw new ValidationError("unknown fs op: " + op);
        }
    }

    /** Structured tool call with response-side caps (mirrors execute_tool). */
    private JsonNode toolRequest(ObjectNode action) {
        JsonNode node = ndjsonRequest(action);
        if (Json.write(node).length > Validation.MAX_GUEST_RESULT_BYTES) {
            throw new TransportError("guest response exceeded limit");
        }
        JsonNode results = node.get("results");
        if (results != null && results.isArray() && results.size() > Validation.MAX_GUEST_RESULTS) {
            throw new RemoteError("guest result limit exceeded");
        }
        return node;
    }

    private JsonNode zbrtFs(int op, String path, ObjectNode args) {
        try (ZbrtConnection conn = openZbrt()) {
            byte[] resp = conn.fs(op, path, Json.write(args));
            return Json.parse(resp);
        }
    }

    private ZbrtConnection openZbrt() {
        return new ZbrtConnection(guestAddress, timeout);
    }

    private JsonNode ndjsonRequest(ObjectNode action) {
        return GuestNdjson.last(GuestNdjson.request(guestAddress, timeout, action), "guest");
    }

    /** RFB durations round up to whole seconds, minimum 1. */
    private static long timeoutSeconds(double timeoutS) {
        return Math.max(1, (long) Math.ceil(timeoutS));
    }

    private static Integer statusCode(JsonNode v, int fallback) {
        JsonNode code = firstOf(v, "exit_code", "status");
        return code != null && code.isNumber() ? code.intValue() : fallback;
    }

    private static JsonNode firstOf(JsonNode v, String a, String b) {
        JsonNode first = v.get(a);
        return first != null ? first : v.get(b);
    }
}
