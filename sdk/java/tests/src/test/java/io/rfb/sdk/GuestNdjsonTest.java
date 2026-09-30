package io.rfb.sdk;

import com.fasterxml.jackson.databind.JsonNode;
import io.rfb.sdk.internal.GuestNdjson;
import io.rfb.sdk.internal.Json;
import io.rfb.sdk.internal.Validation;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.Test;

import java.io.BufferedReader;
import java.io.PrintWriter;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicInteger;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * forkd guest NDJSON client + Sandbox facade against an in-process fake TCP
 * NDJSON server (PROTOCOL.md §2).
 */
class GuestNdjsonTest {
    private FakeNdjsonServer server;

    @AfterEach
    void tearDown() {
        GuestNdjson.agentTokenOverride = null;
        if (server != null) {
            server.close();
        }
    }

    private Sandbox sandbox(FakeNdjsonServer.ConnHandler... handlers) throws Exception {
        server = new FakeNdjsonServer(handlers);
        return sandboxOn(server.address());
    }

    private Sandbox sandboxOn(String address) {
        return sandboxOn(address, 5.0);
    }

    private Sandbox sandboxOn(String address, double timeoutS) {
        RfbClient client = new RfbClient("http://127.0.0.1:1", "", timeoutS);
        SandboxInfo info = new SandboxInfo();
        info.setId("sb-1");
        info.setSnapshotTag("base");
        info.setGuestAddr(address);
        return Sandbox.attach(client, info, RfbClient.TRANSPORT_NDJSON);
    }

    private static void sleepQuietly(long millis) {
        try {
            Thread.sleep(millis);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    /** Reads one request line and replies with the given lines then flushes. */
    private static FakeNdjsonServer.ConnHandler scripted(String... responseLines) {
        return (in, out) -> {
            in.readLine();
            writeLines(out, responseLines);
        };
    }

    private static void writeLines(PrintWriter out, String... lines) {
        for (String line : lines) {
            out.println(line);
        }
        out.flush();
    }

    // ---- wire behavior ----------------------------------------------------

    @Test
    void requestReadsLinesUntilTerminalKey() throws Exception {
        // first two lines carry no terminal key; the third terminates
        server = new FakeNdjsonServer(scripted(
                "{\"event\":\"log\"}",
                "{\"note\":\"partial\"}",
                "{\"out\":\"hi\",\"exit_code\":0}"));
        List<JsonNode> responses = io.rfb.sdk.internal.GuestNdjson.request(
                io.rfb.sdk.internal.GuestNdjson.parseAddress(server.address()),
                java.time.Duration.ofSeconds(5),
                io.rfb.sdk.internal.Json.object().put("action", "exec"));
        assertEquals(3, responses.size());
        assertEquals(0, responses.get(2).get("exit_code").asInt());
    }

    @Test
    void errorLineRaisesImmediately() throws Exception {
        server = new FakeNdjsonServer(scripted("{\"error\":\"no such file\"}"));
        assertThrows(RemoteError.class, () -> io.rfb.sdk.internal.GuestNdjson.request(
                io.rfb.sdk.internal.GuestNdjson.parseAddress(server.address()),
                java.time.Duration.ofSeconds(5),
                io.rfb.sdk.internal.Json.object().put("action", "ping")));
    }

    // ---- facade: exec / eval / ping ----------------------------------------

    @Test
    void execMapsOutAndExitCode() throws Exception {
        Sandbox sandbox = sandbox((in, out) -> {
            JsonNode req = io.rfb.sdk.internal.Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8));
            assertEquals("exec", req.path("action").asText());
            assertEquals("/workspace", req.path("cwd").asText()); // default cwd
            assertEquals(60, req.path("timeout").asLong());       // default timeout 60s
            assertEquals("echo", req.withArray("args").get(0).asText());
            writeLines(out, "{\"out\":\"hi\",\"err\":\"boo\",\"exit_code\":3,\"timed_out\":false}");
        });
        ExecResult result = sandbox.exec(List.of("echo", "hi"));
        assertEquals(Integer.valueOf(3), result.getExitCode());
        assertEquals("hi", result.stdoutText());
        assertEquals("boo", result.stderrText());
        assertFalse(result.isTimedOut());
    }

    @Test
    void execAcceptsStdoutStderrAliases() throws Exception {
        Sandbox sandbox = sandbox(scripted("{\"stdout\":\"o\",\"stderr\":\"e\",\"exit_code\":0}"));
        ExecResult result = sandbox.exec(List.of("x"), "/workspace/sub", 2.5);
        assertEquals("o", result.stdoutText());
        assertEquals("e", result.stderrText());
    }

    @Test
    void execRejectsStdinOverNdjson() throws Exception {
        // The NDJSON exec wire contract has no stdin channel: non-empty stdin
        // would run the command WITHOUT its input, so fail closed (zero wire
        // traffic).
        AtomicInteger requests = new AtomicInteger(0);
        Sandbox sandbox = sandbox((in, out) -> {
            requests.incrementAndGet();
            io.rfb.sdk.internal.Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8));
            writeLines(out, "{\"out\":\"o\",\"exit_code\":0}");
        });
        assertThrows(
                ValidationError.class,
                () -> sandbox.exec(List.of("x"), null, 60.0, "abc".getBytes(StandardCharsets.UTF_8)));
        assertEquals(0, requests.get(), "rejected stdin must not touch the wire");
    }

    @Test
    void evalMapsOutputToStdout() throws Exception {
        Sandbox sandbox = sandbox((in, out) -> {
            JsonNode req = io.rfb.sdk.internal.Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8));
            assertEquals("eval", req.path("action").asText());
            assertEquals("print(1)", req.path("code").asText());
            writeLines(out, "{\"output\":[111,117,116],\"exit_code\":0,\"timed_out\":false}");
        });
        ExecResult result = sandbox.eval("print(1)");
        assertEquals(Integer.valueOf(0), result.getExitCode());
        assertEquals("out", result.stdoutText());
    }

    @Test
    void evalTimeoutRoundsUpToSeconds() throws Exception {
        Sandbox sandbox = sandbox((in, out) -> {
            JsonNode req = io.rfb.sdk.internal.Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8));
            assertEquals(2, req.path("timeout").asLong()); // 1.001s (1001ms) → ceil → 2s
            writeLines(out, "{\"output\":[],\"exit_code\":0}");
        });
        sandbox.eval("x", null, 1.001);
    }

    @Test
    void evalRejectsBlankCodeAndZeroTimeout() throws Exception {
        Sandbox sandbox = sandbox(scripted("{}"));
        assertThrows(ValidationError.class, () -> sandbox.eval("   "));
        assertThrows(ValidationError.class, () -> sandbox.eval("x", null, 0.0));
        assertThrows(ValidationError.class, () -> sandbox.eval("x", "a\\b", null));
    }

    @Test
    void pingReturnsHealthy() throws Exception {
        Sandbox sandbox = sandbox(scripted("{\"pong\":true}"));
        assertTrue(sandbox.ping());
    }

    // ---- facade: filesystem -------------------------------------------------

    @Test
    void lsMapsEntries() throws Exception {
        Sandbox sandbox = sandbox((in, out) -> {
            JsonNode req = io.rfb.sdk.internal.Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8));
            assertEquals("ls", req.path("action").asText());
            assertEquals(".", req.path("path").asText());
            assertEquals(1000, req.path("max_results").asInt());
            writeLines(out, "{\"entries\":[{\"name\":\"a.txt\",\"is_dir\":false,\"size\":3},"
                    + "{\"name\":\"sub\",\"is_dir\":true}],\"truncated\":false}");
        });
        List<DirEntry> entries = sandbox.ls();
        assertEquals(2, entries.size());
        assertEquals("a.txt", entries.get(0).getName());
        assertEquals(Boolean.FALSE, entries.get(0).isDir());
        assertEquals(Long.valueOf(3), entries.get(0).getSize());
        assertEquals(Boolean.TRUE, entries.get(1).isDir());
        assertNull(entries.get(1).getSize());
    }

    @Test
    void findAndGrepMapMatches() throws Exception {
        Sandbox sandboxFind = sandbox((in, out) -> {
            JsonNode req = io.rfb.sdk.internal.Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8));
            assertEquals("find", req.path("action").asText());
            assertEquals("*.txt", req.path("pattern").asText());
            writeLines(out, "{\"matches\":[\"a.txt\",\"b/c.txt\"],\"truncated\":true}");
        });
        List<String> matches = sandboxFind.find("*.txt");
        assertEquals(List.of("a.txt", "b/c.txt"), matches);

        server.close();
        Sandbox sandboxGrep = sandbox((in, out) -> {
            JsonNode req = io.rfb.sdk.internal.Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8));
            assertEquals("grep", req.path("action").asText());
            assertEquals(51200, req.path("max_bytes").asInt());
            writeLines(out, "{\"matches\":[{\"path\":\"a.txt\",\"line\":1,\"column\":2,\"text\":\"x\"}],"
                    + "\"truncated\":false}");
        });
        List<GrepMatch> matches2 = sandboxGrep.grep("x");
        assertEquals(1, matches2.size());
        assertEquals("a.txt", matches2.get(0).getPath());
        assertEquals(Long.valueOf(1), matches2.get(0).getLine());
        assertEquals(Long.valueOf(2), matches2.get(0).getColumn());
        assertEquals("x", matches2.get(0).getText());
    }

    @Test
    void readAndWriteRoundTripWireShapes() throws Exception {
        Sandbox sandboxRead = sandbox((in, out) -> {
            JsonNode req = io.rfb.sdk.internal.Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8));
            assertEquals("read", req.path("action").asText());
            assertEquals("notes.txt", req.path("path").asText());
            assertEquals(5, req.path("offset").asLong());
            writeLines(out, "{\"data\":[104,105],\"truncated\":false,\"total_bytes\":2}");
        });
        FileRead read = sandboxRead.read("notes.txt", 5L, null);
        assertEquals("hi", new String(read.getData(), StandardCharsets.UTF_8));
        assertEquals(Long.valueOf(2), read.getTotalBytes());

        server.close();
        Sandbox sandboxWrite = sandbox((in, out) -> {
            JsonNode req = io.rfb.sdk.internal.Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8));
            assertEquals("write", req.path("action").asText());
            assertEquals("notes.txt", req.path("path").asText());
            assertEquals(Boolean.FALSE, req.path("append").asBoolean());
            JsonNode data = req.get("data");
            assertEquals(2, data.size());
            assertEquals(104, data.get(0).asInt());
            assertEquals(105, data.get(1).asInt());
            writeLines(out, "{\"bytes_written\":2}");
        });
        assertEquals(2, sandboxWrite.write("notes.txt", "hi".getBytes(StandardCharsets.UTF_8)));
    }

    @Test
    void toolResponseOverCapRaises() throws Exception {
        // encoded terminal line > 50 KiB → transport-level size violation
        StringBuilder big = new StringBuilder("{\"matches\":[");
        for (int i = 0; i < 1400; i++) {
            big.append("\"0123456789012345678901234567890123456789\",");
        }
        big.append("\"x\"]}");
        Sandbox sandbox = sandbox(scripted(big.toString()));
        assertThrows(TransportError.class, () -> sandbox.find("x"));
    }

    @Test
    void pathValidationFailClosed() throws Exception {
        Sandbox sandbox = sandbox(scripted("{}")); // must never be reached
        assertThrows(ValidationError.class, () -> sandbox.ls("/etc"));
        assertThrows(ValidationError.class, () -> sandbox.ls("a/../b"));
        assertThrows(ValidationError.class, () -> sandbox.ls("a\\b"));
        assertThrows(ValidationError.class, () -> sandbox.find("x", ""));
        assertThrows(ValidationError.class, () -> sandbox.grep("x", "p".repeat(1025)));
        assertThrows(ValidationError.class, () -> sandbox.read(""));
        assertThrows(ValidationError.class, () -> sandbox.read("C:/x"));
        assertThrows(ValidationError.class, () -> sandbox.write("a\\b", new byte[0]));
        assertThrows(ValidationError.class, () -> sandbox.write("ok", new byte[Validation.MAX_GUEST_RESULT_BYTES + 1]));
        assertThrows(ValidationError.class, () -> sandbox.read("ok", 0L, 0));
        // fail-closed: no connection may have reached the fake guest
        assertEquals(0, server.connectionCount(), "validation must precede any network traffic");
    }

    @Test
    void workspacePrefixAllowedForFsPaths() throws Exception {
        Sandbox sandbox = sandbox(scripted("{\"entries\":[]}"));
        assertEquals(0, sandbox.ls("/workspace/sub").size());
    }

    // ---- facade: stream ------------------------------------------------------

    @Test
    void streamSessionSendInputStopAndTerminal() throws Exception {
        Sandbox sandbox = sandbox((in, out) -> {
            JsonNode req = io.rfb.sdk.internal.Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8));
            assertEquals("stream", req.path("action").asText());
            assertEquals("tail", req.withArray("args").get(0).asText());
            assertEquals(Boolean.TRUE, Boolean.valueOf(req.path("pty").asBoolean()));
            assertEquals("debug", req.path("env").path("LOG").asText());
            writeLines(out, "{\"started\":true}");
            assertEquals("hello", io.rfb.sdk.internal.Json
                    .parse(in.readLine().getBytes(StandardCharsets.UTF_8)).path("in").asText());
            writeLines(out, "{\"out\":\"echo\"}");
            assertEquals("stop", io.rfb.sdk.internal.Json
                    .parse(in.readLine().getBytes(StandardCharsets.UTF_8)).path("action").asText());
            writeLines(out, "{\"exit_code\":0}");
        });
        try (GuestStream stream = sandbox.stream(List.of("tail", "-f", "x"), null, true,
                Map.of("LOG", "debug"))) {
            StreamEvent started = stream.nextEvent();
            assertEquals(StreamEvent.STARTED, started.getKind());
            stream.sendInput("hello");
            StreamEvent chunk = stream.nextEvent();
            assertEquals(StreamEvent.STDOUT, chunk.getKind());
            assertEquals("echo", new String(chunk.getData(), StandardCharsets.UTF_8));
            stream.stop();
            StreamEvent exit = stream.nextEvent();
            assertEquals(StreamEvent.EXIT, exit.getKind());
            assertEquals(Integer.valueOf(0), exit.getCode());
        }
    }

    @Test
    void streamDoneEventMapsWithNullCode() throws Exception {
        Sandbox sandbox = sandbox((in, out) -> {
            in.readLine();
            writeLines(out, "{\"stream\":\"started\"}", "{\"err\":\"boom\"}", "{\"done\":true}");
        });
        try (GuestStream stream = sandbox.stream(List.of("x"))) {
            assertEquals(StreamEvent.STARTED, stream.nextEvent().getKind());
            StreamEvent err = stream.nextEvent();
            assertEquals(StreamEvent.STDERR, err.getKind());
            assertEquals("boom", new String(err.getData(), StandardCharsets.UTF_8));
            StreamEvent exit = stream.nextEvent();
            assertEquals(StreamEvent.EXIT, exit.getKind());
            assertNull(exit.getCode());
        }
    }

    @Test
    void sendInputAfterTerminalRaises() throws Exception {
        Sandbox sandbox = sandbox(scripted("{\"exit_code\":1}"));
        try (GuestStream stream = sandbox.stream(List.of("x"))) {
            assertNotNull(stream.nextEvent()); // exit
            assertThrows(RemoteError.class, () -> stream.sendInput("late"));
            stream.stop(); // idempotent, no raise
        }
    }

    @Test
    void invalidStreamEventRaises() throws Exception {
        Sandbox sandbox = sandbox(scripted("{\"unknown\":1}"));
        try (GuestStream stream = sandbox.stream(List.of("x"))) {
            assertThrows(DecodeError.class, stream::nextEvent);
        }
    }

    // ---- exec read budget (client + deadline + 5s) ---------------------------

    @Test
    void execReadBudgetCoversClientTimeoutPlusDeadlinePlusFive() throws Exception {
        // Cross-language contract §5: a guest answering after the plain client
        // timeout but inside client+exec+5s must succeed (0.5 + 1 + 5 = 6.5s).
        server = new FakeNdjsonServer((in, out) -> {
            in.readLine();
            sleepQuietly(1200);
            writeLines(out, "{\"out\":\"late\",\"exit_code\":0}");
        });
        Sandbox sandbox = sandboxOn(server.address(), 0.5);
        ExecResult result = sandbox.exec(List.of("echo"), null, 1.0);
        assertEquals("late", result.stdoutText());
        assertEquals(Integer.valueOf(0), result.getExitCode());
    }

    @Test
    void nonExecRequestsKeepThePlainClientTimeout() throws Exception {
        // The widened budget is exec-only: ls still times out at the client
        // timeout instead of waiting an extra 5s+deadline.
        server = new FakeNdjsonServer((in, out) -> {
            in.readLine();
            sleepQuietly(1500);
            writeLines(out, "{\"entries\":[]}");
        });
        Sandbox sandbox = sandboxOn(server.address(), 0.4);
        long start = System.nanoTime();
        assertThrows(TransportError.class, sandbox::ls);
        assertTrue((System.nanoTime() - start) < 1_500_000_000L, "ls must not use the exec budget");
    }

    // ---- agent auth (FORKD_AGENT_TOKEN) --------------------------------------

    @Test
    void agentAuthLinePrecedesExecWhenTokenConfigured() throws Exception {
        GuestNdjson.agentTokenOverride = "sekret";
        Sandbox sandbox = sandbox((in, out) -> {
            JsonNode auth = Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8));
            assertEquals("auth", auth.path("action").asText());
            assertEquals("sekret", auth.path("token").asText());
            writeLines(out, "{\"action\":\"auth\",\"ok\":true}");
            JsonNode exec = Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8));
            assertEquals("exec", exec.path("action").asText());
            writeLines(out, "{\"out\":\"hi\",\"exit_code\":0}");
        });
        ExecResult result = sandbox.exec(List.of("echo", "hi"));
        assertEquals("hi", result.stdoutText());
    }

    @Test
    void agentAuthLinePrecedesStreamWhenTokenConfigured() throws Exception {
        GuestNdjson.agentTokenOverride = "sekret";
        Sandbox sandbox = sandbox((in, out) -> {
            assertEquals("auth", Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8))
                    .path("action").asText());
            writeLines(out, "{\"action\":\"auth\",\"ok\":true}");
            assertEquals("stream", Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8))
                    .path("action").asText());
            writeLines(out, "{\"started\":true}", "{\"exit_code\":0}");
        });
        try (GuestStream stream = sandbox.stream(List.of("cat"))) {
            assertEquals(StreamEvent.STARTED, stream.nextEvent().getKind());
            assertEquals(StreamEvent.EXIT, stream.nextEvent().getKind());
        }
    }

    @Test
    void agentAuthRejectionIsRemoteError() throws Exception {
        GuestNdjson.agentTokenOverride = "wrong-token";
        Sandbox sandbox = sandbox((in, out) -> {
            in.readLine(); // the auth line
            writeLines(out, "{\"error\":\"authentication failed\"}");
        });
        RemoteError error = assertThrows(RemoteError.class, sandbox::ping);
        assertTrue(error.getMessage().contains("authentication failed"));
    }

    @Test
    void agentUnexpectedAuthReplyIsRemoteError() throws Exception {
        GuestNdjson.agentTokenOverride = "sekret";
        Sandbox sandbox = sandbox((in, out) -> {
            in.readLine();
            writeLines(out, "{\"pong\":true}"); // not an auth ack
        });
        assertThrows(RemoteError.class, sandbox::ping);
    }

    @Test
    void agentAuthDisabledWithoutToken() throws Exception {
        Sandbox sandbox = sandbox((in, out) -> {
            assertEquals("ping", Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8))
                    .path("action").asText());
            writeLines(out, "{\"pong\":true}");
        });
        assertTrue(sandbox.ping());
    }

    @Test
    void agentBlankTokenDisablesAuth() throws Exception {
        GuestNdjson.agentTokenOverride = "   ";
        Sandbox sandbox = sandbox((in, out) -> {
            assertEquals("ping", Json.parse(in.readLine().getBytes(StandardCharsets.UTF_8))
                    .path("action").asText());
            writeLines(out, "{\"pong\":true}");
        });
        assertTrue(sandbox.ping());
    }

}
