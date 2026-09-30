package io.rfb.sdk;

import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import io.rfb.sdk.internal.ControllerHttp;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** forkd controller client against an in-process fake HTTP server. */
class ControllerHttpTest {
    private HttpServer server;
    private String baseUrl;
    private final AtomicReference<String> lastAuthHeader = new AtomicReference<>(null);
    private final AtomicReference<String> lastCreateBody = new AtomicReference<>(null);
    private final AtomicReference<String> snapshotListBody = new AtomicReference<>("[]");
    private final AtomicInteger polls = new AtomicInteger(0);
    private volatile boolean flipToReady = false;
    private volatile int infoStatus = 200;
    private volatile String infoBody = "{\"tag\":\"base\",\"status\":\"Ready\",\"bootable\":true}";
    private volatile int legacyStatus = 404;

    @BeforeEach
    void setUp() throws IOException {
        server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        baseUrl = "http://127.0.0.1:" + server.getAddress().getPort();

        server.createContext("/v1/snapshots", ex -> {
            lastAuthHeader.set(ex.getRequestHeaders().getFirst("Authorization"));
            String path = ex.getRequestURI().getPath();
            if ("/v1/snapshots".equals(path)) {
                String body = snapshotListBody.get();
                if (flipToReady && polls.incrementAndGet() > 1) {
                    body = "[{\"tag\":\"base\",\"status\":\"ready\",\"bootable\":true}]";
                }
                respond(ex, 200, body.getBytes(StandardCharsets.UTF_8));
            } else {
                // legacy /v1/snapshots/{tag}
                respond(ex, legacyStatus, infoBody.getBytes(StandardCharsets.UTF_8));
            }
        });
        server.createContext("/v1/snapshots/base/info", ex ->
                respond(ex, infoStatus, infoBody.getBytes(StandardCharsets.UTF_8)));
        server.createContext("/v1/sandboxes", ex -> {
            String path = ex.getRequestURI().getPath();
            if ("/v1/sandboxes".equals(path) && "POST".equals(ex.getRequestMethod())) {
                lastCreateBody.set(new String(readBody(ex), StandardCharsets.UTF_8));
                respond(ex, 200, sandboxArray());
            } else if ("/v1/sandboxes".equals(path)) {
                respond(ex, 200, sandboxArray());
            } else if (path.endsWith("/ping")) {
                respond(ex, 200, "{\"ok\":true}".getBytes(StandardCharsets.UTF_8));
            } else if ("DELETE".equals(ex.getRequestMethod())) {
                if (path.endsWith("/sb-404")) {
                    respond(ex, 404, "{\"error\":\"gone\"}".getBytes(StandardCharsets.UTF_8));
                } else if (path.endsWith("/sb-500")) {
                    respond(ex, 500, "{\"error\":\"boom\"}".getBytes(StandardCharsets.UTF_8));
                } else {
                    respond(ex, 200, "null".getBytes(StandardCharsets.UTF_8));
                }
            } else {
                respond(ex, 404, "{\"error\":\"no route\"}".getBytes(StandardCharsets.UTF_8));
            }
        });
        server.start();
    }

    private static byte[] sandboxArray() {
        return ("[{\"id\":\"sb-1\",\"snapshot_tag\":\"base\",\"guest_addr\":\"127.0.0.1:7021\","
                + "\"created_at_unix\":null,\"netns\":null,\"memory_limit_mib\":null,\"pid\":null,"
                + "\"has_branched\":false,\"branch_count\":0}]").getBytes(StandardCharsets.UTF_8);
    }

    @AfterEach
    void tearDown() {
        server.stop(0);
    }

    private static void respond(HttpExchange ex, int status, byte[] body) throws IOException {
        ex.getResponseHeaders().set("Content-Type", "application/json");
        ex.sendResponseHeaders(status, body.length == 0 ? -1 : body.length);
        try (OutputStream out = ex.getResponseBody()) {
            out.write(body);
        }
    }

    private static byte[] readBody(HttpExchange ex) throws IOException {
        try (InputStream in = ex.getRequestBody()) {
            return in.readAllBytes();
        }
    }

    @Test
    void listSnapshotsParsesArrayWithDefaults() {
        snapshotListBody.set(
                "[{\"tag\":\"base\",\"status\":\"ready\",\"bootable\":true,\"created_at_unix\":17}]");
        ControllerHttp http = new ControllerHttp(baseUrl, null, Duration.ofSeconds(5));
        List<Snapshot> snapshots = http.listSnapshots();
        assertEquals(1, snapshots.size());
        Snapshot s = snapshots.get(0);
        assertEquals("base", s.getTag());
        assertEquals("ready", s.getStatus());
        assertTrue(s.isBootable());
        assertEquals(Long.valueOf(17), s.getCreatedAtUnix());
        assertEquals("", s.getDir()); // serde(default) default
        assertNull(s.getWarning());
        assertNull(s.getProvenance());
    }

    @Test
    void snapshotInfoPreferredEndpointWins() {
        infoStatus = 200;
        infoBody = "{\"tag\":\"base\",\"status\":\"ready\",\"bootable\":true}";
        ControllerHttp http = new ControllerHttp(baseUrl, null, Duration.ofSeconds(5));
        Snapshot snapshot = http.snapshotInfo("base");
        assertNotNull(snapshot);
        assertEquals("ready", snapshot.getStatus());
    }

    @Test
    void snapshotInfoFallsBackToLegacyOn404() {
        infoStatus = 404;
        legacyStatus = 200;
        infoBody = "{\"tag\":\"base\",\"status\":\"ready\",\"bootable\":true}";
        ControllerHttp http = new ControllerHttp(baseUrl, null, Duration.ofSeconds(5));
        Snapshot snapshot = http.snapshotInfo("base");
        assertNotNull(snapshot);
        assertEquals("base", snapshot.getTag());
    }

    @Test
    void snapshotInfoBoth404YieldsNull() {
        infoStatus = 404;
        legacyStatus = 404;
        infoBody = "{\"error\":\"missing\"}";
        ControllerHttp http = new ControllerHttp(baseUrl, null, Duration.ofSeconds(5));
        assertNull(http.snapshotInfo("base"));
    }

    @Test
    void non2xxMapsToJsonErrorField() {
        infoStatus = 500;
        infoBody = "{\"error\":\"info failed\"}";
        ControllerHttp http = new ControllerHttp(baseUrl, null, Duration.ofSeconds(5));
        HttpStatusError error = assertThrows(HttpStatusError.class, () -> http.snapshotInfo("base"));
        assertEquals(500, error.getStatus());
        assertTrue(error.getMessage().contains("info failed"));
    }

    @Test
    void non2xxWithoutErrorFieldUsesBodyPrefix() {
        infoStatus = 502;
        infoBody = "bad gateway body";
        ControllerHttp http = new ControllerHttp(baseUrl, null, Duration.ofSeconds(5));
        HttpStatusError error = assertThrows(HttpStatusError.class, () -> http.snapshotInfo("base"));
        assertTrue(error.getMessage().contains("bad gateway body"));
    }

    @Test
    void bearerTokenHeaderOnlyWhenTokenSet() {
        new ControllerHttp(baseUrl, "secret", Duration.ofSeconds(5)).listSnapshots();
        assertEquals("Bearer secret", lastAuthHeader.get());

        lastAuthHeader.set(null);
        new ControllerHttp(baseUrl, "", Duration.ofSeconds(5)).listSnapshots();
        assertNull(lastAuthHeader.get());
    }

    @Test
    void deleteSandboxTreats404AsSuccess() {
        ControllerHttp http = new ControllerHttp(baseUrl, null, Duration.ofSeconds(5));
        http.deleteSandbox("sb-1");      // 200
        http.deleteSandbox("sb-404");    // 404 → success
        HttpStatusError error = assertThrows(HttpStatusError.class, () -> http.deleteSandbox("sb-500"));
        assertEquals(500, error.getStatus());
    }

    @Test
    void sandboxIdValidationFailClosed() {
        ControllerHttp http = new ControllerHttp(baseUrl, null, Duration.ofSeconds(5));
        assertThrows(ValidationError.class, () -> http.pingSandbox(""));
        assertThrows(ValidationError.class, () -> http.pingSandbox("a".repeat(129)));
        assertThrows(ValidationError.class, () -> http.deleteSandbox("../etc/passwd"));
        assertThrows(ValidationError.class, () -> http.deleteSandbox("sb 1"));
        assertTrue(http.pingSandbox("sb-1").path("ok").asBoolean());
    }

    @Test
    void createSandboxSerializesExactFieldNames() {
        ControllerHttp http = new ControllerHttp(baseUrl, null, Duration.ofSeconds(5));
        List<SandboxInfo> created = http.createSandbox("base", 2, true, null, false, true, false);
        assertEquals(1, created.size());
        assertEquals("sb-1", created.get(0).getId());
        assertEquals("127.0.0.1:7021", created.get(0).getGuestAddr());
        assertEquals("{\"snapshot_tag\":\"base\",\"n\":2,\"per_child_netns\":true,"
                        + "\"memory_limit_mib\":null,\"prewarm\":false,\"live_fork\":true,\"hugepages\":false}",
                lastCreateBody.get());
    }

    @Test
    void waitLoopReturnsWhenReadyAndFailsFastOnFailed() {
        // not ready → times out after the short budget
        snapshotListBody.set("[{\"tag\":\"base\",\"status\":\"creating\",\"bootable\":false}]");
        RfbClient client = new RfbClient(baseUrl, null, 5.0);
        assertThrows(RfbError.class, () -> client.waitSnapshot("base", 0.3));

        // failed → raises immediately (well under 1s)
        snapshotListBody.set("[{\"tag\":\"base\",\"status\":\"Failed\",\"bootable\":false}]");
        long start = System.nanoTime();
        assertThrows(RfbError.class, () -> client.waitSnapshot("base", 5));
        assertTrue((System.nanoTime() - start) < 1_000_000_000L, "failed status must raise immediately");
    }

    @Test
    void waitLoopReturnsSnapshotOnceReady() {
        flipToReady = true; // first poll "creating", second poll ready
        snapshotListBody.set("[{\"tag\":\"base\",\"status\":\"creating\",\"bootable\":false}]");
        Snapshot snapshot = new RfbClient(baseUrl, null, 5.0).waitSnapshot("base", 5);
        assertEquals("base", snapshot.getTag());
        assertTrue(snapshot.isBootable());
    }


    @Test
    void snapshotArrayDecodeRejectsNonArray() {
        snapshotListBody.set("{\"tag\":\"base\"}");
        ControllerHttp http = new ControllerHttp(baseUrl, null, Duration.ofSeconds(5));
        assertThrows(DecodeError.class, http::listSnapshots);
    }

    @Test
    void transportErrorOnUnreachableServer() {
        ControllerHttp http = new ControllerHttp("http://127.0.0.1:1", null, Duration.ofSeconds(1));
        assertThrows(TransportError.class, http::listSnapshots);
    }

    @Test
    void rejectsInvalidBaseUrl() {
        assertThrows(ValidationError.class,
                () -> new ControllerHttp("ftp://x", null, Duration.ofSeconds(5)));
        assertThrows(ValidationError.class,
                () -> new ControllerHttp("not a url", null, Duration.ofSeconds(5)));
        assertThrows(ValidationError.class,
                () -> new ControllerHttp(baseUrl, null, Duration.ZERO));
    }

    @Test
    void snapshotProvenanceRoundTripsOpaqueJson() {
        snapshotListBody.set("[{\"tag\":\"base\",\"status\":\"ready\",\"bootable\":true,"
                + "\"provenance\":{\"k\":[1,2],\"s\":\"v\"}}]");
        ControllerHttp http = new ControllerHttp(baseUrl, null, Duration.ofSeconds(5));
        Snapshot s = http.listSnapshots().get(0);
        assertNotNull(s.getProvenance());
        assertTrue(String.valueOf(s.getProvenance()).contains("k"));
    }
}
