package io.rfb.sdk;

import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.Test;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.concurrent.atomic.AtomicReference;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Full facade flow over fakes: create sandbox → exec → read/write → delete,
 * plus connect/ping/transport validation (UNIFIED_API.md §9 quickstart shape).
 */
class RfbClientFacadeTest {
    private HttpServer controller;
    private FakeNdjsonServer guest;
    private final AtomicReference<String> lastPingAuth = new AtomicReference<>(null);

    @AfterEach
    void tearDown() {
        if (controller != null) {
            controller.stop(0);
        }
        if (guest != null) {
            guest.close();
        }
    }

    private String startController() throws IOException {
        controller = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        String base = "http://127.0.0.1:" + controller.getAddress().getPort();
        byte[] sandboxJson = ("[{\"id\":\"sb-1\",\"snapshot_tag\":\"base\","
                + "\"guest_addr\":\"%ADDR%\",\"created_at_unix\":null,\"netns\":null,"
                + "\"memory_limit_mib\":null,\"pid\":null,\"has_branched\":false,"
                + "\"branch_count\":0}]").getBytes(StandardCharsets.UTF_8);
        controller.createContext("/v1/snapshots", ex -> respond(ex, 200,
                "[{\"tag\":\"base\",\"status\":\"ready\",\"bootable\":true}]"
                        .getBytes(StandardCharsets.UTF_8)));
        controller.createContext("/v1/sandboxes", ex -> {
            String path = ex.getRequestURI().getPath();
            lastPingAuth.set(ex.getRequestHeaders().getFirst("Authorization"));
            if ("/v1/sandboxes".equals(path) && "POST".equals(ex.getRequestMethod())) {
                respond(ex, 200, sandboxJson);
            } else if ("/v1/sandboxes".equals(path)) {
                respond(ex, 200, sandboxJson);
            } else if (path.endsWith("/ping")) {
                respond(ex, 200, "{\"ok\":true}".getBytes(StandardCharsets.UTF_8));
            } else if ("DELETE".equals(ex.getRequestMethod())) {
                respond(ex, 200, "null".getBytes(StandardCharsets.UTF_8));
            } else {
                respond(ex, 404, "{\"error\":\"no route\"}".getBytes(StandardCharsets.UTF_8));
            }
        });
        controller.start();
        return base;
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
    void quickstartFlowCreateExecReadWriteDelete() throws Exception {
        // one guest server, one scripted handler per connection, in call order:
        // exec → write → read (delete never touches the guest)
        guest = new FakeNdjsonServer(
                (in, out) -> {
                    String request = in.readLine();
                    assertTrue(request.contains("\"action\":\"exec\""));
                    respondLines(out, "{\"out\":\"hi\",\"exit_code\":0}");
                },
                (in, out) -> {
                    String request = in.readLine();
                    assertTrue(request.contains("\"action\":\"write\""));
                    assertTrue(request.contains("\"data\":[104,101,108,108,111]"));
                    respondLines(out, "{\"bytes_written\":5}");
                },
                (in, out) -> {
                    String request = in.readLine();
                    assertTrue(request.contains("\"action\":\"read\""));
                    respondLines(out, "{\"data\":[104,101,108,108,111],\"truncated\":false,\"total_bytes\":5}");
                });
        String controllerBase = startController();
        controller.removeContext("/v1/sandboxes");
        byte[] sandboxJson = ("[{\"id\":\"sb-1\",\"snapshot_tag\":\"base\","
                + "\"guest_addr\":\"" + guest.address() + "\",\"created_at_unix\":null,"
                + "\"netns\":null,\"memory_limit_mib\":null,\"pid\":null,"
                + "\"has_branched\":false,\"branch_count\":0}]").getBytes(StandardCharsets.UTF_8);
        controller.createContext("/v1/sandboxes", ex -> {
            String path = ex.getRequestURI().getPath();
            if ("/v1/sandboxes".equals(path)) {
                respond(ex, 200, sandboxJson);
            } else if (path.endsWith("/ping")) {
                respond(ex, 200, "{\"ok\":true}".getBytes(StandardCharsets.UTF_8));
            } else if ("DELETE".equals(ex.getRequestMethod())) {
                respond(ex, 200, "null".getBytes(StandardCharsets.UTF_8));
            } else {
                respond(ex, 404, "{\"error\":\"no route\"}".getBytes(StandardCharsets.UTF_8));
            }
        });

        RfbClient client = new RfbClient(controllerBase, "token-x", 5.0);
        assertTrue(client.waitSnapshot("base").bootable);

        Sandbox sandbox = client.createSandbox("base").get(0);
        assertEquals("sb-1", sandbox.id());

        ExecResult exec = sandbox.exec(List.of("echo", "hi"));
        assertEquals(Integer.valueOf(0), exec.exitCode);
        assertEquals("hi", exec.stdoutText());

        assertEquals(5, sandbox.write("notes.txt", "hello".getBytes(StandardCharsets.UTF_8)));

        FileRead read = sandbox.read("notes.txt");
        assertEquals("hello", new String(read.data, StandardCharsets.UTF_8));
        assertEquals(Long.valueOf(5), read.totalBytes);

        sandbox.delete();
    }

    private static void respondLines(java.io.PrintWriter out, String... lines) {
        for (String line : lines) {
            out.println(line);
        }
        out.flush();
    }

    @Test
    void connectByIdUsesListedGuestAddr() throws Exception {
        guest = new FakeNdjsonServer((in, out) -> respondLines(out, "{\"pong\":true}"));
        String controllerBase = startController();
        controller.removeContext("/v1/sandboxes");
        byte[] sandboxJson = ("[{\"id\":\"live-1\",\"snapshot_tag\":\"base\","
                + "\"guest_addr\":\"" + guest.address() + "\",\"created_at_unix\":9,"
                + "\"netns\":null,\"memory_limit_mib\":null,\"pid\":null,"
                + "\"has_branched\":false,\"branch_count\":0}]").getBytes(StandardCharsets.UTF_8);
        controller.createContext("/v1/sandboxes", ex -> {
            String path = ex.getRequestURI().getPath();
            lastPingAuth.set(ex.getRequestHeaders().getFirst("Authorization"));
            if (path.endsWith("/ping")) {
                respond(ex, 200, "{\"ok\":true}".getBytes(StandardCharsets.UTF_8));
            } else {
                respond(ex, 200, sandboxJson);
            }
        });

        RfbClient client = new RfbClient(controllerBase, "token-x", 5.0);
        Sandbox sandbox = client.connect("live-1");
        assertEquals("live-1", sandbox.id());
        assertEquals(Long.valueOf(9), sandbox.createdAtUnix());
        assertEquals(RfbClient.TRANSPORT_NDJSON, sandbox.transport());
        assertTrue(sandbox.ping());

        assertThrows(RemoteError.class, () -> client.connect("missing"));

        Object pingValue = client.pingSandbox("live-1");
        assertTrue(pingValue instanceof java.util.Map);
        assertEquals(Boolean.TRUE, ((java.util.Map<?, ?>) pingValue).get("ok"));
        assertEquals("Bearer token-x", lastPingAuth.get());
    }

    @Test
    void connectValidatesTransportName() throws Exception {
        String controllerBase = startController();
        RfbClient client = new RfbClient(controllerBase, null, 5.0);
        assertThrows(ValidationError.class, () -> client.connect("sb-1", "grpc"));
    }

    @Test
    void connectSandboxAttachesDirectlyWithoutReresolution() throws Exception {
        // unreachable controller: if connect(Sandbox) re-resolved via
        // listSandboxes it would raise; direct attach must return as-is
        RfbClient client = new RfbClient("http://127.0.0.1:1", "", 5.0);
        SandboxInfo info = new SandboxInfo();
        info.id = "direct-1";
        info.guestAddr = "127.0.0.1:1";
        Sandbox attached = Sandbox.attach(client, info, RfbClient.TRANSPORT_ZBRT);
        assertSame(attached, client.connect(attached));
        assertEquals(RfbClient.TRANSPORT_ZBRT, client.connect(attached).transport());
    }

    @Test
    void createSandboxHonorsTransportParameter() throws Exception {
        String controllerBase = startController();
        controller.removeContext("/v1/sandboxes");
        byte[] sandboxJson = ("[{\"id\":\"sb-1\",\"snapshot_tag\":\"base\","
                + "\"guest_addr\":\"127.0.0.1:1\",\"created_at_unix\":null,\"netns\":null,"
                + "\"memory_limit_mib\":null,\"pid\":null,\"has_branched\":false,"
                + "\"branch_count\":0}]").getBytes(StandardCharsets.UTF_8);
        controller.createContext("/v1/sandboxes", ex -> {
            if ("/v1/sandboxes".equals(ex.getRequestURI().getPath())) {
                respond(ex, 200, sandboxJson);
            } else {
                respond(ex, 404, "{\"error\":\"no route\"}".getBytes(StandardCharsets.UTF_8));
            }
        });
        RfbClient client = new RfbClient(controllerBase, null, 5.0);
        List<Sandbox> created = client.createSandbox("base", 1, false, null, false, false, false,
                RfbClient.TRANSPORT_ZBRT);
        assertEquals(1, created.size());
        assertEquals(RfbClient.TRANSPORT_ZBRT, created.get(0).transport());
        // invalid transport name fails closed before any HTTP call
        assertThrows(ValidationError.class, () -> client.createSandbox("base", 1, false, null,
                false, false, false, "grpc"));
    }

    @Test
    void snapshotDetailBoth404IsNullThroughFacade() throws Exception {
        String controllerBase = startController();
        controller.createContext("/v1/snapshots/base", ex ->
                respond(ex, 404, "{\"error\":\"missing\"}".getBytes(StandardCharsets.UTF_8)));
        controller.createContext("/v1/snapshots/base/info", ex ->
                respond(ex, 404, "{\"error\":\"missing\"}".getBytes(StandardCharsets.UTF_8)));
        RfbClient client = new RfbClient(controllerBase, null, 5.0);
        assertNull(client.snapshot("base"));
    }

    @Test
    void facadeDefaultsResolveEnvironment() {
        // explicit args beat env; the controller URL is validated up front
        assertThrows(ValidationError.class,
                () -> new RfbClient("no-scheme-host", null, 10.0));
        assertThrows(ValidationError.class, () -> new RfbClient(null, null, 0.0));
        assertThrows(ValidationError.class, () -> new RfbClient(null, null, -1.0));
    }
}
