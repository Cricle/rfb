package io.rfb.sdk;

import com.fasterxml.jackson.databind.JsonNode;
import io.rfb.sdk.internal.ControllerHttp;

import java.time.Duration;
import java.util.ArrayList;
import java.util.List;

/**
 * The single public client of the RFB Java SDK (UNIFIED_API.md). A mirror port
 * of the Rust reference implementation {@code rfb::client::RfbClient}:
 *
 * <pre>{@code
 * RfbClient client = new RfbClient();
 * Snapshot snap = client.waitSnapshot("base");
 * Sandbox box = client.createSandbox("base").get(0);
 * ExecResult r = box.exec(java.util.List.of("echo", "hi"));
 * box.write("notes.txt", "hello".getBytes());
 * FileRead back = box.read("notes.txt");
 * box.delete();
 * }</pre>
 *
 * <p>Defaults: {@code baseUrl=null} → env {@code FORKD_URL} (else
 * {@code http://127.0.0.1:8889}); {@code token=null} → env {@code FORKD_TOKEN}
 * (sent as {@code Authorization: Bearer} when non-blank); timeout 10s.
 */
public final class RfbClient {
    /** Transport choice for {@link #connect}: NDJSON (default) or ZBRT v1 frames. */
    public static final String TRANSPORT_NDJSON = "ndjson";
    public static final String TRANSPORT_ZBRT = "zbrt";

    private final ControllerHttp controller;
    private final double timeoutS;

    /** Client from environment variables ({@code FORKD_URL}, {@code FORKD_TOKEN}). */
    public RfbClient() {
        this(null, null, 10.0);
    }

    /**
     * @param baseUrl  forkd controller base URL; null → env/default
     * @param token    bearer token; null → env {@code FORKD_TOKEN}
     * @param timeoutS request timeout in seconds
     */
    public RfbClient(String baseUrl, String token, double timeoutS) {
        if (!(timeoutS > 0) || Double.isNaN(timeoutS) || Double.isInfinite(timeoutS)) {
            throw new ValidationError("timeout must be a positive, finite number of seconds");
        }
        String effectiveToken = token == null
                ? ControllerHttp.envToken()
                : (token.trim().isEmpty() ? null : token);
        this.controller = new ControllerHttp(
                baseUrl != null ? baseUrl : ControllerHttp.envUrl(),
                effectiveToken,
                Duration.ofMillis((long) (timeoutS * 1000)));
        this.timeoutS = timeoutS;
    }

    /** Request timeout in seconds this client was built with. */
    public double getTimeoutS() {
        return timeoutS;
    }

    // ---- snapshots -------------------------------------------------------

    /** GET /v1/snapshots. */
    public List<Snapshot> listSnapshots() {
        return controller.listSnapshots();
    }

    /**
     * Snapshot detail or null. Prefers {@code /v1/snapshots/{tag}/info}; a 404
     * falls back to legacy {@code /v1/snapshots/{tag}}; both 404 → null.
     */
    public Snapshot snapshot(String tag) {
        return controller.snapshotInfo(tag);
    }

    /** Poll every 100ms until ready (default 60s budget). */
    public Snapshot waitSnapshot(String tag) {
        return waitSnapshot(tag, 60.0);
    }

    /**
     * Poll {@link #listSnapshots()} every 100ms until the snapshot is ready and
     * bootable. A "failed" status raises {@link RemoteError} immediately; a
     * timeout raises {@link TransportError} (UNIFIED_API.md §7: timeouts are
     * transport-class errors).
     */
    public Snapshot waitSnapshot(String tag, double timeoutS) {
        long deadline = System.nanoTime() + (long) (timeoutS * 1_000_000_000L);
        while (true) {
            for (Snapshot s : controller.listSnapshots()) {
                if (s.tag.equals(tag)) {
                    if (s.status.equalsIgnoreCase("failed")) {
                        throw new RemoteError("forkd snapshot `" + tag + "` is Failed");
                    }
                    if (s.status.equalsIgnoreCase("ready") && s.bootable) {
                        return s;
                    }
                }
            }
            long remaining = deadline - System.nanoTime();
            if (remaining <= 0) {
                throw new TransportError(
                        "forkd snapshot `" + tag + "` did not become Ready before timeout");
            }
            long sleepMs = Math.min(100, remaining / 1_000_000);
            if (sleepMs > 0) {
                try {
                    Thread.sleep(sleepMs);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    throw new TransportError("wait interrupted", e);
                }
            }
        }
    }

    // ---- sandboxes -------------------------------------------------------

    /** Create one sandbox from the snapshot (all other options defaulted). */
    public List<Sandbox> createSandbox(String snapshotTag) {
        return createSandbox(snapshotTag, TRANSPORT_NDJSON);
    }

    /**
     * Create one sandbox from the snapshot with the given guest transport for
     * the returned facades ({@code "ndjson"} default or {@code "zbrt"}).
     */
    public List<Sandbox> createSandbox(String snapshotTag, String transport) {
        return createSandbox(snapshotTag, 1, false, null, false, false, false, transport);
    }

    /** Create one or more sandboxes; the returned facades use the NDJSON guest transport. */
    public List<Sandbox> createSandbox(String snapshotTag, int n, boolean perChildNetns,
                                       Long memoryLimitMib, boolean prewarm, boolean liveFork,
                                       boolean hugepages) {
        return createSandbox(snapshotTag, n, perChildNetns, memoryLimitMib, prewarm, liveFork,
                hugepages, TRANSPORT_NDJSON);
    }

    /**
     * Create one or more sandboxes. Parameters mirror the Rust
     * {@code CreateOptions}: {@code n} sandboxes, optional per-child netns,
     * memory limit (MiB), prewarm, live-fork and hugepages. {@code transport}
     * ({@code "ndjson"} or {@code "zbrt"}) selects the guest transport of the
     * returned {@link Sandbox} facades (default NDJSON).
     */
    public List<Sandbox> createSandbox(String snapshotTag, int n, boolean perChildNetns,
                                       Long memoryLimitMib, boolean prewarm, boolean liveFork,
                                       boolean hugepages, String transport) {
        if (!TRANSPORT_NDJSON.equals(transport) && !TRANSPORT_ZBRT.equals(transport)) {
            throw new ValidationError("transport must be \"ndjson\" or \"zbrt\"");
        }
        return attachAll(controller.createSandbox(
                snapshotTag, n, perChildNetns, memoryLimitMib, prewarm, liveFork, hugepages),
                transport);
    }

    /** List the live sandbox pool. */
    public List<Sandbox> listSandboxes() {
        return attachAll(controller.listSandboxes(), TRANSPORT_NDJSON);
    }

    /** Attach to an existing sandbox by id (NDJSON transport). */
    public Sandbox connect(String sandboxId) {
        return connect(sandboxId, TRANSPORT_NDJSON);
    }

    /**
     * Attach to an existing sandbox by id with the given guest transport
     * ({@code "ndjson"} or {@code "zbrt"}).
     */
    public Sandbox connect(String sandboxId, String transport) {
        return Sandbox.connectById(this, sandboxId, transport);
    }

    /**
     * Attach to an existing sandbox object as-is (keeps its transport), like
     * the Rust {@code ConnectTarget for &Sandbox}. Only a {@link String} id is
     * re-resolved through {@link #listSandboxes()}.
     */
    public Sandbox connect(Sandbox sandbox) {
        return sandbox;
    }

    /** Ping a sandbox; returns the controller's JSON value (Map/List/String/Number/Boolean/null). */
    public Object pingSandbox(String sandboxId) {
        JsonNode node = controller.pingSandbox(sandboxId);
        return io.rfb.sdk.internal.Json.MAPPER.convertValue(node, Object.class);
    }

    /** Delete a sandbox; both 2xx and 404 are success. */
    public void deleteSandbox(String sandboxId) {
        controller.deleteSandbox(sandboxId);
    }

    ControllerHttp controller() {
        return controller;
    }

    private List<Sandbox> attachAll(List<SandboxInfo> infos, String transport) {
        List<Sandbox> sandboxes = new ArrayList<>(infos.size());
        for (SandboxInfo info : infos) {
            sandboxes.add(Sandbox.attach(this, info, transport));
        }
        return sandboxes;
    }
}
