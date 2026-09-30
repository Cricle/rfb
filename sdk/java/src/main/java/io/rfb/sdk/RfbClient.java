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
 * ExecResult r = box.exec(java.util.Arrays.asList("echo", "hi"));
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

    /** ZBRT (ZeroBoot v1) guest transport selector for {@link #connect}. */
    public static final String TRANSPORT_ZBRT = "zbrt";

    /** Wait-loop ceiling (24h): nanos-conversion overflow protection. */
    private static final double MAX_WAIT_TIMEOUT_S = 86_400;

    private final ControllerHttp controller;
    private final double timeoutS;

    /**
     * Client from environment variables: {@code baseUrl} and {@code token}
     * resolve from {@code FORKD_URL} / {@code FORKD_TOKEN} with the UNIFIED_API
     * §2 defaults, timeout 10s.
     *
     * @throws ValidationError the resolved URL is not a valid http(s) URL with host
     */
    public RfbClient() {
        this(null, null, 10.0);
    }

    /**
     * Client with explicit settings. A {@code null} {@code baseUrl} resolves
     * from env {@code FORKD_URL} (blank falls back to
     * {@code http://127.0.0.1:8889}); a {@code null} {@code token} resolves
     * from env {@code FORKD_TOKEN} and is only sent (as
     * {@code Authorization: Bearer}) when non-blank.
     *
     * @param baseUrl  forkd controller base URL; null → env {@code FORKD_URL}
     * @param token    bearer token; null → env {@code FORKD_TOKEN}
     * @param timeoutS per-request (connect + read) timeout in seconds, {@code > 0}
     * @throws ValidationError invalid URL or non-positive/NaN/infinite timeout
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

    /**
     * All snapshots as reported by the controller ({@code GET /v1/snapshots}).
     *
     * @return every snapshot, in controller order
     * @throws TransportError connection/read failure or request timeout
     * @throws HttpStatusError the controller returned a non-2xx status
     */
    public List<Snapshot> listSnapshots() {
        return controller.listSnapshots();
    }

    /**
     * Snapshot detail for one tag. Prefers {@code /v1/snapshots/{tag}/info}; a
     * 404 falls back to the legacy {@code /v1/snapshots/{tag}} endpoint; when
     * both return 404 the snapshot is unknown.
     *
     * @param tag snapshot tag to look up
     * @return the snapshot detail, or {@code null} when both endpoints 404
     * @throws TransportError  connection/read failure or request timeout
     * @throws HttpStatusError a non-404 non-2xx status was returned
     */
    public Snapshot snapshot(String tag) {
        return controller.snapshotInfo(tag);
    }

    /**
     * Poll {@link #listSnapshots()} every 100ms until the snapshot is ready and
     * bootable, with the default 60s budget (UNIFIED_API.md §8).
     *
     * @param tag snapshot tag to wait for
     * @return the ready, bootable snapshot
     * @throws RemoteError    the snapshot reports a {@code failed} status
     * @throws TransportError the 60s budget elapsed (timeouts are transport-class)
     */
    public Snapshot waitSnapshot(String tag) {
        return waitSnapshot(tag, 60.0);
    }

    /**
     * Poll {@link #listSnapshots()} every 100ms until the snapshot is ready and
     * bootable. A {@code failed} status raises {@link RemoteError} immediately; a
     * timeout raises {@link TransportError} (UNIFIED_API.md §7: timeouts are
     * transport-class errors).
     *
     * @param tag      snapshot tag to wait for
     * @param timeoutS wait budget in seconds ({@code > 0}, finite)
     * @return the ready, bootable snapshot
     * @throws ValidationError non-finite or non-positive {@code timeoutS}
     * @throws RemoteError     the snapshot reports a {@code failed} status
     * @throws TransportError  the budget elapsed before the snapshot turned ready
     */
    public Snapshot waitSnapshot(String tag, double timeoutS) {
        if (Double.isNaN(timeoutS) || Double.isInfinite(timeoutS) || timeoutS <= 0) {
            throw new ValidationError("timeoutS must be a positive, finite number of seconds");
        }
        // Clamp BEFORE the nanos conversion: `timeoutS * 1e9` overflows long
        // for huge inputs (unit mix-ups — ms passed as s), which would move
        // the deadline into the past and fake an instant timeout.
        long deadline = System.nanoTime()
                + (long) (Math.min(timeoutS, MAX_WAIT_TIMEOUT_S) * 1_000_000_000L);
        while (true) {
            for (Snapshot s : controller.listSnapshots()) {
                if (s.getTag().equals(tag)) {
                    if (s.getStatus().equalsIgnoreCase("failed")) {
                        throw new RemoteError("forkd snapshot `" + tag + "` is Failed");
                    }
                    if (s.getStatus().equalsIgnoreCase("ready") && s.isBootable()) {
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

    /**
     * Create one sandbox from the snapshot (all other options defaulted, NDJSON
     * guest transport).
     *
     * @param snapshotTag snapshot to boot the sandbox from
     * @return a single-element list with the new sandbox facade
     * @throws TransportError  connection/read failure or request timeout
     * @throws HttpStatusError the controller returned a non-2xx status
     */
    public List<Sandbox> createSandbox(String snapshotTag) {
        return createSandbox(snapshotTag, TRANSPORT_NDJSON);
    }

    /**
     * Create one sandbox from the snapshot with the given guest transport for
     * the returned facades ({@code "ndjson"} default or {@code "zbrt"}).
     *
     * @param snapshotTag snapshot to boot the sandbox from
     * @param transport   {@code "ndjson"} or {@code "zbrt"}
     * @return a single-element list with the new sandbox facade
     * @throws ValidationError unknown transport name (fail closed)
     * @throws TransportError  connection/read failure or request timeout
     * @throws HttpStatusError the controller returned a non-2xx status
     */
    public List<Sandbox> createSandbox(String snapshotTag, String transport) {
        return createSandbox(snapshotTag, 1, false, null, false, false, false, transport);
    }

    /**
     * Create one or more sandboxes; the returned facades use the NDJSON guest transport.
     *
     * @param snapshotTag    snapshot to boot the sandboxes from
     * @param n              number of sandboxes to create
     * @param perChildNetns  give each sandbox its own network namespace
     * @param memoryLimitMib memory cap in MiB ({@code null} = controller default)
     * @param prewarm        request prewarmed microVMs
     * @param liveFork       create via live fork
     * @param hugepages      use hugepages for guest memory
     * @return the new sandbox facades (one per created sandbox)
     * @throws TransportError  connection/read failure or request timeout
     * @throws HttpStatusError the controller returned a non-2xx status
     */
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
     *
     * @param snapshotTag    snapshot to boot the sandboxes from
     * @param n              number of sandboxes to create
     * @param perChildNetns  give each sandbox its own network namespace
     * @param memoryLimitMib memory cap in MiB ({@code null} = controller default)
     * @param prewarm        request prewarmed microVMs
     * @param liveFork       create via live fork
     * @param hugepages      use hugepages for guest memory
     * @param transport      {@code "ndjson"} or {@code "zbrt"}
     * @return the new sandbox facades (one per created sandbox)
     * @throws ValidationError unknown transport name (fail closed)
     * @throws TransportError  connection/read failure or request timeout
     * @throws HttpStatusError the controller returned a non-2xx status
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

    /**
     * List the live sandbox pool.
     *
     * @return every live sandbox, wrapped in NDJSON-transport facades
     * @throws TransportError  connection/read failure or request timeout
     * @throws HttpStatusError the controller returned a non-2xx status
     */
    public List<Sandbox> listSandboxes() {
        return attachAll(controller.listSandboxes(), TRANSPORT_NDJSON);
    }

    /**
     * Attach to an existing sandbox by id (NDJSON transport). The id is
     * resolved through {@link #listSandboxes()}.
     *
     * @param sandboxId controller-assigned sandbox id
     * @return a facade attached to the live sandbox
     * @throws ValidationError malformed sandbox id or transport
     * @throws RemoteError     no live sandbox with that id ("sandbox not found")
     * @throws TransportError  connection/read failure or request timeout
     */
    public Sandbox connect(String sandboxId) {
        return connect(sandboxId, TRANSPORT_NDJSON);
    }

    /**
     * Attach to an existing sandbox by id with the given guest transport
     * ({@code "ndjson"} or {@code "zbrt"}). The id is resolved through
     * {@link #listSandboxes()}.
     *
     * @param sandboxId controller-assigned sandbox id
     * @param transport {@code "ndjson"} or {@code "zbrt"}
     * @return a facade attached to the live sandbox
     * @throws ValidationError malformed sandbox id or unknown transport
     * @throws RemoteError     no live sandbox with that id ("sandbox not found")
     * @throws TransportError  connection/read failure or request timeout
     */
    public Sandbox connect(String sandboxId, String transport) {
        return Sandbox.connectById(this, sandboxId, transport);
    }

    /**
     * Attach to an existing sandbox object as-is (keeps its transport), like
     * the Rust {@code ConnectTarget for &Sandbox}. Only a {@link String} id is
     * re-resolved through {@link #listSandboxes()}.
     *
     * @param sandbox an existing sandbox facade
     * @return the attach target (the same facade, unchanged)
     */
    public Sandbox connect(Sandbox sandbox) {
        return sandbox;
    }

    /**
     * Controller-level ping for a sandbox id.
     *
     * @param sandboxId controller-assigned sandbox id
     * @return the controller's JSON ping value
     *         (Map/List/String/Number/Boolean/null)
     * @throws ValidationError malformed sandbox id
     * @throws HttpStatusError non-2xx controller status
     * @throws TransportError  connection/read failure or request timeout
     */
    public Object pingSandbox(String sandboxId) {
        JsonNode node = controller.pingSandbox(sandboxId);
        return io.rfb.sdk.internal.Json.MAPPER.convertValue(node, Object.class);
    }

    /**
     * Delete a sandbox; both 2xx and 404 are success (UNIFIED_API.md §3).
     *
     * @param sandboxId controller-assigned sandbox id
     * @throws ValidationError malformed sandbox id
     * @throws HttpStatusError non-2xx, non-404 controller status
     * @throws TransportError  connection/read failure or request timeout
     */
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
