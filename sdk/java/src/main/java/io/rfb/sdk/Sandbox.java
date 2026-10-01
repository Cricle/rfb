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
import java.util.Arrays;
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
public final class Sandbox implements AutoCloseable {
    private final RfbClient client;
    private final SandboxInfo info;
    private final String transport;
    private final InetSocketAddress guestAddress;
    private final Duration timeout;
    /** health/fs RPC 复用的控制连接（rust zbrt.rs 语义）。 */
    /** exec 温连接池（已 Hello 的空闲连接）。 */
    private final java.util.concurrent.ConcurrentLinkedQueue<ZbrtConnection>
            zbrtExecPool = new java.util.concurrent.ConcurrentLinkedQueue<>();

    private Sandbox(RfbClient client, SandboxInfo info, String transport) {
        this.client = client;
        this.info = info;
        this.transport = transport;
        this.guestAddress = GuestNdjson.parseAddress(info.getGuestAddr());
        this.timeout = Duration.ofMillis((long) (client.getTimeoutS() * 1000));
    }

    /**
     * Attach a sandbox at a KNOWN guest address with an explicit transport —
     * the entry point for direct ZBRT bridges (no controller involved).
     *
     * @param client    owning client (supplies the guest timeout)
     * @param info      controller sandbox metadata carrying the guest address
     * @param transport {@code "ndjson"} or {@code "zbrt"}
     * @return a facade over the sandbox's guest
     */
    public static Sandbox attach(RfbClient client, SandboxInfo info, String transport) {
        return new Sandbox(client, info, transport);
    }

    /**
     * Resolve a sandbox by id against the live pool and attach with the given
     * transport.
     *
     * @throws ValidationError unknown transport or malformed sandbox id
     * @throws RemoteError     no live sandbox with that id ("sandbox not found")
     */
    static Sandbox connectById(RfbClient client, String sandboxId, String transport) {
        if (!RfbClient.TRANSPORT_NDJSON.equals(transport) && !RfbClient.TRANSPORT_ZBRT.equals(transport)) {
            throw new ValidationError("transport must be \"ndjson\" or \"zbrt\"");
        }
        // Fail closed before any HTTP traffic: the id travels in a URL path.
        Validation.sandboxId(sandboxId);
        for (SandboxInfo info : client.controller().listSandboxes()) {
            if (info.getId().equals(sandboxId)) {
                return new Sandbox(client, info, transport);
            }
        }
        throw new RemoteError("sandbox not found: " + sandboxId);
    }

    // ---- properties ------------------------------------------------------

    /** @return the controller-assigned sandbox id. */
    public String id() {
        return info.getId();
    }

    /** @return the tag of the snapshot this sandbox was created from. */
    public String snapshotTag() {
        return info.getSnapshotTag();
    }

    /** @return host:port of the guest agent. */
    public String guestAddr() {
        return info.getGuestAddr();
    }

    /** @return creation time in Unix seconds, or {@code null} when unknown. */
    public Long createdAtUnix() {
        return info.getCreatedAtUnix();
    }

    /** @return the controller sandbox metadata backing this facade. */
    public SandboxInfo info() {
        return info;
    }

    /** Guest transport of this facade: {@code "ndjson"} or {@code "zbrt"}. */
    public String transport() {
        return transport;
    }

    // ---- health ----------------------------------------------------------

    /**
     * Guest liveness probe. NDJSON: healthy only when the reply's {@code pong}
     * field is present and JSON-true (the {@code healthy} field does not
     * count; unknown reply keys are ignored). ZBRT: the HealthAck's
     * {@code healthy} flag (UNIFIED_API.md §4).
     *
     * @return {@code true} only when the agent reports healthy
     * @throws TransportError connection/read failure or timeout
     * @throws RemoteError    the guest replied with an {@code error} line/frame
     */
    public boolean ping() {
        if (RfbClient.TRANSPORT_ZBRT.equals(transport)) {
            return zbrtControl(ZbrtConnection::health).healthy();
        }
        // Rust baseline (ndjson::ping_healthy): healthy only when the pong
        // flag is present and literally JSON-true.
        JsonNode pong = ndjsonRequest(Json.object().put("action", "ping")).get("pong");
        return pong != null && pong.isBoolean() && pong.booleanValue();
    }

    // ---- exec / eval -----------------------------------------------------

    /**
     * Execute one command with the defaults: cwd {@code /workspace}, 60s
     * timeout, no stdin.
     *
     * @param args command argv; must be non-empty
     * @return the exec result
     * @throws ValidationError empty argv (fail closed)
     */
    public ExecResult exec(List<String> args) {
        return exec(args, null);
    }

    /**
     * Execute one command with a custom cwd (60s timeout, no stdin).
     *
     * @param args command argv; must be non-empty
     * @param cwd  guest working directory ({@code /workspace} when null)
     * @return the exec result
     * @throws ValidationError empty argv or invalid cwd
     */
    public ExecResult exec(List<String> args, String cwd) {
        return exec(args, cwd, 60.0);
    }

    /**
     * Execute one command with a custom cwd and deadline (no stdin).
     *
     * @param args     command argv; must be non-empty
     * @param cwd      guest working directory ({@code /workspace} when null)
     * @param timeoutS per-request deadline in seconds ({@code > 0})
     * @return the exec result
     * @throws ValidationError empty argv, invalid cwd, or bad timeout
     */
    public ExecResult exec(List<String> args, String cwd, double timeoutS) {
        return exec(args, cwd, timeoutS, null);
    }

    /**
     * Execute one command and wait for its completion. {@code cwd} null
     * resolves to {@code /workspace} on both transports (UNIFIED_API.md §8 —
     * the agent rejects {@code /} as cwd). {@code stdin} is delivered over the
     * ZBRT transport only; the NDJSON wire contract has no exec stdin channel,
     * so non-empty stdin fails closed there (running the command without its
     * input would be silent data loss).
     *
     * @param args     command argv; must be non-empty
     * @param cwd      guest working directory ({@code /workspace} when null)
     * @param timeoutS per-request deadline in seconds ({@code > 0})
     * @param stdin    bytes fed to the command's stdin (ZBRT only; ignored-empty elsewhere)
     * @return the aggregated exec result
     * @throws ValidationError empty argv, bad cwd/timeout, non-empty stdin over
     *                         NDJSON, or (ZBRT) argv over 255 entries / stdin
     *                         over 16 MiB — all before any frame is sent
     * @throws RemoteError     the guest reported an error or exceeded the 16 MiB turn cap
     * @throws TransportError  connect/read/write failure or timeout
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
        // §8: the four-language default cwd is /workspace (the agent rejects /).
        String effectiveCwd = cwd != null ? cwd : "/workspace";
        if (RfbClient.TRANSPORT_ZBRT.equals(transport)) {
            // Fail closed BEFORE any connection: ZBRT encodes argc in a single
            // byte and payloads are u32-bounded, so an oversized argv/stdin is
            // a local ValidationError with zero frames (UNIFIED_API.md §4/§9.8).
            Validation.zbrtArgs(args);
            Validation.payloadSize(stdin == null ? 0 : stdin.length,
                    Validation.MAX_ZBRT_PAYLOAD_BYTES);
            // The ZBRT deadline travels as whole seconds (ceil) times 1000,
            // not truncated milliseconds; beyond the u32 wire range it clamps
            // to the maximum (mirrors the Rust baseline's u32::MAX saturation).
            long timeoutMs = Math.min(
                    (long) Math.ceil(timeoutS) * 1000, 0xFFFFFFFFL);
            // The guest needs the full deadline to surface its own timeout
            // error: widen the socket budget like the NDJSON exec path
            // (client timeout + deadline + 5 s margin).
            return execViaPool(args, effectiveCwd, stdin, timeoutMs, timeoutS);
        }
        // The NDJSON exec wire contract has no stdin channel: non-empty stdin
        // would run the command WITHOUT its input, so fail closed (delivered
        // only over ZBRT; mirrors the Rust baseline).
        if (stdin != null && stdin.length > 0) {
            throw new ValidationError("stdin is only supported over the ZBRT transport");
        }
        ObjectNode action = Json.object()
                .put("action", "exec")
                .put("cwd", effectiveCwd)
                .put("timeout", timeoutSeconds(timeoutS));
        com.fasterxml.jackson.databind.node.ArrayNode argv = action.withArray("args");
        for (String arg : args) {
            argv.add(arg);
        }
        JsonNode v = ndjsonRequest(action, timeoutS);
        return new ExecResult(
                statusCode(v, -1),
                // UNIFIED_API.md §4: the current keys win when both are present.
                Json.valueBytes(firstOf(v, "stdout", "out")),
                Json.valueBytes(firstOf(v, "stderr", "err")),
                v.path("timed_out").asBoolean(false));
    }

    /**
     * Evaluate a code snippet with the defaults (no cwd override, no deadline).
     *
     * @param code snippet to evaluate; non-blank, at most 1 MiB of UTF-8
     * @return the eval result (output mapped to stdout, stderr always empty)
     * @throws ValidationError blank/oversized code, or eval over ZBRT (fail closed)
     */
    public ExecResult eval(String code) {
        return eval(code, null);
    }

    /**
     * Evaluate a code snippet with an optional cwd override (no deadline).
     *
     * @param code snippet to evaluate; non-blank, at most 1 MiB of UTF-8
     * @param cwd  guest working directory ({@code null} = agent default)
     * @return the eval result (output mapped to stdout, stderr always empty)
     * @throws ValidationError blank/oversized code, invalid cwd, or eval over ZBRT
     */
    public ExecResult eval(String code, String cwd) {
        return eval(code, cwd, null);
    }

    /**
     * Evaluate a code snippet in the guest. Eval output maps to
     * {@link ExecResult#getStdout()}; stderr is always empty and the exit code
     * comes from the agent's {@code status} (legacy {@code exit_code} accepted,
     * missing/non-integer → 0). NDJSON carries {@code {"action":"eval"}};
     * over ZBRT the call fails closed with {@link ValidationError} (ZBRT v1
     * has no eval opcode and the reference guest would run a literal
     * {@code eval <code>} command) — see {@code sdk/shared/README.md §1}.
     *
     * @param code     snippet to evaluate; non-blank, at most 1 MiB of UTF-8
     * @param cwd      guest working directory ({@code null} = agent default)
     * @param timeoutS optional deadline in seconds ({@code > 0} when present)
     * @return the eval result
     * @throws ValidationError blank/oversized code, invalid cwd, bad timeout,
     *                         or eval over ZBRT — all before any frame is sent
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
            // ZBRT v1 has no eval opcode and the reference guest maps Execute
            // verbatim onto `exec` — fail closed instead of running a literal
            // `eval <code>` command (mirrors the Rust facade).
            throw new ValidationError("eval is not supported over the ZBRT transport");
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
                statusCode(v, 0, "status", "exit_code"),
                Json.valueBytes(firstOf(v, "output", "out")),
                new byte[0],
                v.path("timed_out").asBoolean(false));
    }

    // ---- filesystem ------------------------------------------------------

    /**
     * List directory entries at the guest's default path ({@code "."}).
     *
     * @return the entries under the path (at most 1000)
     * @throws ValidationError invalid path
     * @throws DecodeError     the reply carried no {@code entries} array
     */
    public List<DirEntry> ls() {
        return ls(".");
    }

    /**
     * List directory entries under {@code path}.
     *
     * @param path guest fs path (relative or {@code /workspace}-prefixed)
     * @return the entries under the path (at most 1000)
     * @throws ValidationError invalid path
     * @throws DecodeError     the reply carried no {@code entries} array
     */
    public List<DirEntry> ls(String path) {
        Validation.fsPath(path);
        JsonNode node = toolOrFs(1, path, Json.object().put("max_results", Validation.MAX_GUEST_RESULTS));
        JsonNode entriesNode = node.get("entries");
        if (entriesNode == null || !entriesNode.isArray()) {
            throw new DecodeError("guest response is missing entries");
        }
        List<DirEntry> entries = new ArrayList<>();
        for (JsonNode entry : entriesNode) {
            entries.add(new DirEntry(
                    entry.path("name").asText(""),
                    entry.path("is_dir").asBoolean(false),
                    entry.hasNonNull("size") ? entry.get("size").asLong() : null));
        }
        return entries;
    }

    /**
     * Find files by name pattern at the guest's default path ({@code "."}).
     *
     * @param pattern glob pattern (non-empty, at most 1024 bytes)
     * @return matching guest paths (at most 1000)
     * @throws ValidationError invalid path or pattern
     * @throws DecodeError     the reply carried no string {@code matches} array
     */
    public List<String> find(String pattern) {
        return find(".", pattern);
    }

    /**
     * Find files by name pattern under {@code path}.
     *
     * @param path    guest fs path (relative or {@code /workspace}-prefixed)
     * @param pattern glob pattern (non-empty, at most 1024 bytes)
     * @return matching guest paths (at most 1000)
     * @throws ValidationError invalid path or pattern
     * @throws DecodeError     the reply carried no string {@code matches} array
     */
    public List<String> find(String path, String pattern) {
        Validation.fsPath(path);
        Validation.pattern(pattern);
        JsonNode node = toolOrFs(2, path, Json.object()
                .put("max_results", Validation.MAX_GUEST_RESULTS)
                .put("pattern", pattern));
        JsonNode matchesNode = node.get("matches");
        if (matchesNode == null || !matchesNode.isArray()) {
            throw new DecodeError("guest response is missing matches");
        }
        List<String> matches = new ArrayList<>();
        for (JsonNode match : matchesNode) {
            if (!match.isTextual()) {
                throw new DecodeError("find matches must be strings");
            }
            matches.add(match.asText());
        }
        return matches;
    }

    /**
     * Grep file contents at the guest's default path ({@code "."}).
     *
     * @param pattern regex/glob pattern (non-empty, at most 1024 bytes)
     * @return the matches (at most 1000, 50 KiB of match text)
     * @throws ValidationError invalid path or pattern
     * @throws DecodeError     the reply carried no {@code matches} array
     */
    public List<GrepMatch> grep(String pattern) {
        return grep(".", pattern);
    }

    /**
     * Grep file contents under {@code path}.
     *
     * @param path    guest fs path (relative or {@code /workspace}-prefixed)
     * @param pattern regex/glob pattern (non-empty, at most 1024 bytes)
     * @return the matches (at most 1000, 50 KiB of match text)
     * @throws ValidationError invalid path or pattern
     * @throws DecodeError     the reply carried no {@code matches} array
     */
    public List<GrepMatch> grep(String path, String pattern) {
        Validation.fsPath(path);
        Validation.pattern(pattern);
        JsonNode node = toolOrFs(3, path, Json.object()
                .put("max_results", Validation.MAX_GUEST_RESULTS)
                .put("pattern", pattern)
                .put("max_bytes", Validation.MAX_GUEST_RESULT_BYTES));
        JsonNode matchesNode = node.get("matches");
        if (matchesNode == null || !matchesNode.isArray()) {
            throw new DecodeError("guest response is missing matches");
        }
        List<GrepMatch> matches = new ArrayList<>();
        for (JsonNode match : matchesNode) {
            matches.add(new GrepMatch(
                    match.path("path").asText(""),
                    match.hasNonNull("line") ? match.get("line").asLong() : null,
                    match.hasNonNull("column") ? match.get("column").asLong() : null,
                    match.path("text").asText("")));
        }
        return matches;
    }

    /**
     * Read a whole guest file (backend byte cap applies).
     *
     * @param path guest file path (no NUL/backslash/{@code ..}, non-host path)
     * @return the file bytes plus truncation info
     * @throws ValidationError invalid path
     */
    public FileRead read(String path) {
        return read(path, null, null);
    }

    /**
     * Read a guest file with optional offset and byte cap.
     *
     * @param path     guest file path (no NUL/backslash/{@code ..}, non-host path)
     * @param offset   byte offset to start at ({@code null} = start of file)
     * @param maxBytes cap on returned bytes, 1..51200 ({@code null} = backend default)
     * @return the file bytes plus truncation info
     *         ({@link FileRead#getTotalBytes()} reports the file size)
     * @throws ValidationError invalid path or out-of-range {@code maxBytes}
     */
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

    /**
     * Write (replace) a guest file.
     *
     * @param path guest file path (no NUL/backslash/{@code ..}, non-host path)
     * @param data bytes to write (at most 51200)
     * @return the number of bytes written
     * @throws ValidationError invalid path or oversized payload
     * @throws DecodeError     the reply carried no {@code bytes_written}
     */
    public int write(String path, byte[] data) {
        return write(path, data, false, null);
    }

    /**
     * Write or append to a guest file.
     *
     * @param path   guest file path (no NUL/backslash/{@code ..}, non-host path)
     * @param data   bytes to write (at most 51200)
     * @param append append instead of truncating the file
     * @param mode   file mode bits ({@code null} = agent default)
     * @return the number of bytes written
     * @throws ValidationError invalid path or oversized payload
     * @throws DecodeError     the reply carried no {@code bytes_written}
     */
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
            JsonNode written = zbrtFs(5, path, args);
            if (!written.hasNonNull("bytes_written")) {
                throw new DecodeError("guest response is missing bytes_written");
            }
            return written.get("bytes_written").asInt();
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
        JsonNode written = toolRequest(action);
        if (!written.hasNonNull("bytes_written")) {
            throw new DecodeError("guest response is missing bytes_written");
        }
        return (int) written.get("bytes_written").asLong();
    }

    // ---- stream ----------------------------------------------------------

    /**
     * Open an interactive stream with no cwd, pty or env overrides.
     *
     * @param args command argv; must be non-empty
     * @return the interactive stream handle
     * @throws ValidationError empty argv or invalid cwd
     */
    public GuestStream stream(List<String> args) {
        return stream(args, null, null, null);
    }

    /**
     * Open an interactive stream (UNIFIED_API.md §5). {@code cwd} is an
     * optional guest path. Empty argv is rejected on both transports, and over
     * ZBRT {@code pty}/{@code env} fail closed with {@link ValidationError}
     * before any frame is sent (ZBRT v1 has neither channel) — mirroring the
     * Rust/C#/Python baselines.
     *
     * @param args command argv; must be non-empty
     * @param cwd  guest working directory (null = agent default)
     * @param pty  allocate a pseudo-terminal (NDJSON only; {@code true} rejected over ZBRT)
     * @param env  extra environment variables (NDJSON only; non-empty rejected over ZBRT)
     * @return the interactive stream handle
     * @throws ValidationError empty argv, bad cwd, or a ZBRT-unsupported option / argv size
     * @throws TransportError  connection failure
     */
    public GuestStream stream(List<String> args, String cwd, Boolean pty, Map<String, String> env) {
        if (args == null || args.isEmpty()) {
            throw new ValidationError("args must not be empty");
        }
        if (cwd != null) {
            Validation.filePath(cwd);
        }
        if (RfbClient.TRANSPORT_ZBRT.equals(transport)) {
            // Rust baseline (client/facade.rs GuestOps::stream, ZBRT arm):
            // pty / env / oversized argv are fail-closed rejected before any
            // frame is sent (and before the connection is even opened).
            if (Boolean.TRUE.equals(pty)) {
                throw new ValidationError("pty is not supported over the ZBRT transport");
            }
            if (env != null && !env.isEmpty()) {
                throw new ValidationError("env is not supported over the ZBRT transport");
            }
            Validation.zbrtArgs(args);
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

    /**
     * Delete this sandbox via the controller; both 2xx and 404 are success
     * (UNIFIED_API.md §3/§4).
     *
     * @throws HttpStatusError non-2xx, non-404 controller status
     * @throws TransportError  connection/read failure or request timeout
     */
    public void delete() {
        ZbrtConnection pooled;
        while ((pooled = zbrtExecPool.poll()) != null) {
            pooled.close();
        }
        client.deleteSandbox(info.getId());
    }

    /** AutoCloseable — deletes the sandbox (2xx/404 both succeed);
     * try-with-resources guarantees cleanup on a mid-flow failure. */
    @Override
    public void close() {
        delete();
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
        return Json.parse(zbrtControl(conn -> conn.fs(op, path, Json.write(args))));
    }

    /** 统一温池（health/fs 与 exec 共用，rust zbrt.rs 的重试分类）：借出时
     * Hello 验活；写失败 / EOF 截断 = 请求未送达，换新连接重试一次；读超时
     * （请求可能已执行）与解码/guest 错误绝不重试。 */
    private <T> T zbrtControl(java.util.function.Function<ZbrtConnection, T> op) {
        for (int attempt = 0; attempt < 2; attempt++) {
            boolean pooled = attempt == 0;
            ZbrtConnection conn = pooled ? borrowExecConn(timeout) : null;
            if (conn == null) {
                pooled = false;
                conn = openZbrt(timeout);
            }
            try {
                T out = op.apply(conn);
                repayExecConn(conn);
                return out;
            } catch (RfbError e) {
                conn.close();
                if (pooled && zbrtRetryable(e)) {
                    continue;
                }
                throw e;
            }
        }
        throw new TransportError("zbrt exchange exhausted");
    }

    /** exec 温连接池：已 Hello 的空闲连接，借还复用（借出先重发 Hello 验活
     * ——死的丢弃继续找/回退新连接）；写失败 = 请求未送达，换新连接重试
     * 一次；读侧失败绝不重试。池连接的 socket 预算在借出时刷新。 */
    private ExecResult execViaPool(List<String> args, String cwd, byte[] stdin,
                                   long timeoutMs, double timeoutS) {
        ZbrtConnection conn = borrowExecConn(execReadBudget(timeoutS));
        boolean pooled = conn != null;
        if (conn == null) {
            conn = openZbrt(execReadBudget(timeoutS));
        }
        try {
            ZbrtConnection.Exec exec =
                    conn.execute(args, cwd, stdin, timeoutMs);
            repayExecConn(conn);
            return new ExecResult(exec.code(), exec.stdout(), exec.stderr(),
                    exec.timedOut());
        } catch (RfbError e) {
            conn.close();
            if (pooled && e instanceof TransportError
                    && e.getMessage() != null
                    && e.getMessage().startsWith("zbrt write")) {
                ZbrtConnection fresh = openZbrt(execReadBudget(timeoutS));
                try {
                    ZbrtConnection.Exec exec = fresh.execute(
                            args, cwd, stdin, timeoutMs);
                    repayExecConn(fresh);
                    return new ExecResult(exec.code(), exec.stdout(),
                            exec.stderr(), exec.timedOut());
                } catch (RfbError e2) {
                    fresh.close();
                    throw e2;
                }
            }
            throw e;
        }
    }

    private ZbrtConnection borrowExecConn(Duration socketBudget) {
        while (true) {
            ZbrtConnection conn = zbrtExecPool.poll();
            if (conn == null) {
                return null;
            }
            try {
                conn.hello(ZbrtConnection.CLIENT_NAME);
                return conn;
            } catch (RfbError e) {
                conn.close();
            }
        }
    }

    private void repayExecConn(ZbrtConnection conn) {
        if (zbrtExecPool.size() < 4) {
            zbrtExecPool.offer(conn);
        } else {
            conn.close();
        }
    }

    private static boolean zbrtRetryable(RfbError e) {
        if (e instanceof RemoteError || e instanceof ValidationError) {
            return false;
        }
        if (e instanceof DecodeError) {
            return e.getMessage() != null
                    && e.getMessage().startsWith("truncated frame");
        }
        if (e instanceof TransportError) {
            return !(e.getCause() instanceof java.net.SocketTimeoutException);
        }
        return false;
    }


    private ZbrtConnection openZbrt() {
        return openZbrt(timeout);
    }

    /** Open a ZBRT connection with an explicit socket budget. */
    private ZbrtConnection openZbrt(Duration socketTimeout) {
        return new ZbrtConnection(guestAddress, socketTimeout);
    }

    private JsonNode ndjsonRequest(ObjectNode action) {
        return GuestNdjson.last(GuestNdjson.request(guestAddress, timeout, action), "guest");
    }

    /** Fixed margin on top of the exec read budget (Python {@code _guest.py} baseline). */
    private static final long NDJSON_EXEC_READ_MARGIN_MS = 5_000L;

    /**
     * NDJSON exec read budget: client timeout + exec deadline + 5 s
     * (cross-language contract §5, mirroring Python {@code _effective_timeout})
     * so the guest's own timeout error surfaces instead of the client's.
     */
    private JsonNode ndjsonRequest(ObjectNode action, double execTimeoutS) {
        return GuestNdjson.last(GuestNdjson.request(guestAddress, execReadBudget(execTimeoutS), action),
                "guest");
    }

    private Duration execReadBudget(double execTimeoutS) {
        return timeout
                .plusMillis((long) Math.ceil(execTimeoutS * 1000.0))
                .plusMillis(NDJSON_EXEC_READ_MARGIN_MS);
    }

    /** RFB durations round up to whole seconds, minimum 1. */
    private static long timeoutSeconds(double timeoutS) {
        return Math.max(1, (long) Math.ceil(timeoutS));
    }

    private static Integer statusCode(JsonNode v, int fallback) {
        return statusCode(v, fallback, "exit_code", "status");
    }

    /**
     * Current-key-first per UNIFIED_API.md §4: exec answers with
     * {@code exit_code}, eval with {@code status} (PROTOCOL.md §2.4) — the
     * caller names the pair in precedence order. A NON-INTEGER number (e.g.
     * 1.5 from a misbehaving agent) is a contract violation: it maps to the
     * fallback (-1), never truncates to a fake success code.
     */
    private static Integer statusCode(JsonNode v, int fallback, String current, String legacy) {
        JsonNode code = firstOf(v, current, legacy);
        return code != null && code.isIntegralNumber() ? code.intValue() : fallback;
    }

    private static JsonNode firstOf(JsonNode v, String a, String b) {
        JsonNode first = v.get(a);
        return first != null ? first : v.get(b);
    }
}
