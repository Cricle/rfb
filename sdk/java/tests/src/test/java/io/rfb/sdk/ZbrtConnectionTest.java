package io.rfb.sdk;

import io.rfb.sdk.internal.ZbrtCodec;
import io.rfb.sdk.internal.ZbrtConnection;
import io.rfb.sdk.internal.ZbrtFrame;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.Test;

import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * ZBRT client against an in-process fake ZBRT server (PROTOCOL.md §3.4
 * session semantics: streamed Output → exactly one terminal, idempotent
 * Cancel → empty-payload CancelAck, Health → HealthAck, Error raise).
 */
class ZbrtConnectionTest {
    private FakeZbrtServer server;

    @AfterEach
    void tearDown() {
        if (server != null) {
            server.close();
        }
    }

    private ZbrtConnection open(FakeZbrtServer.ConnHandler handler) throws Exception {
        server = new FakeZbrtServer(handler);
        return new ZbrtConnection(
                io.rfb.sdk.internal.GuestNdjson.parseAddress(server.address()),
                java.time.Duration.ofSeconds(5));
    }

    @Test
    void helloReturnsServerNameAndCapabilities() throws Exception {
        try (ZbrtConnection conn = open(io -> {
            ZbrtFrame hello = io.read();
            assertEquals(ZbrtFrame.KIND_HELLO, hello.kind());
            io.write(io.reply(hello, ZbrtFrame.KIND_HELLO_ACK,
                    ZbrtCodec.encodeHelloAck("rfb-zeroboot-guest", ZbrtConnection.V1_CAPABILITIES)));
        })) {
            ZbrtCodec.HelloAck ack = conn.hello("sdk-test");
            assertEquals("rfb-zeroboot-guest", ack.server);
            assertEquals(ZbrtConnection.V1_CAPABILITIES, ack.capabilities);
        }
    }

    @Test
    void executeCollectsOutputStreamsThenExit() throws Exception {
        try (ZbrtConnection conn = open(io -> {
            ZbrtFrame exec = io.read();
            assertEquals(ZbrtFrame.KIND_EXECUTE, exec.kind());
            // encodeExecute: argc + texts + cwd flag + stdin + timeout — spot check
            io.writeFrames(
                    FakeZbrtServer.outputFrame(exec.requestId(), 0, "hi".getBytes(StandardCharsets.UTF_8)),
                    FakeZbrtServer.outputFrame(exec.requestId(), 1, "err".getBytes(StandardCharsets.UTF_8)),
                    FakeZbrtServer.exitFrame(exec.requestId(), 0));
        })) {
            ZbrtConnection.Exec exec = conn.execute(List.of("echo", "hi"), "/workspace",
                    "abc".getBytes(StandardCharsets.UTF_8), 1500);
            assertEquals(0, exec.code());
            assertEquals("hi", new String(exec.stdout(), StandardCharsets.UTF_8));
            assertEquals("err", new String(exec.stderr(), StandardCharsets.UTF_8));
            assertTrue(!exec.timedOut());
        }
    }

    @Test
    void errorFrameRaisesRemoteError() throws Exception {
        try (ZbrtConnection conn = open(io -> {
            ZbrtFrame exec = io.read();
            io.write(FakeZbrtServer.errorFrame(exec.requestId(), 1, "argv is empty"));
        })) {
            RemoteError error = assertThrows(RemoteError.class,
                    () -> conn.execute(List.of(), null, new byte[0], 0));
            assertEquals(1, error.getCode());
            assertTrue(error.getMessage().contains("argv is empty"));
        }
    }

    @Test
    void secondExecuteAndDuplicateRequestRaise() throws Exception {
        AtomicInteger counter = new AtomicInteger(0);
        try (ZbrtConnection conn = open(io -> {
            while (true) {
                ZbrtFrame exec = io.read();
                int n = counter.incrementAndGet();
                if (n == 1) {
                    io.write(FakeZbrtServer.exitFrame(exec.requestId(), 0));
                } else if (n == 2) {
                    io.write(FakeZbrtServer.errorFrame(exec.requestId(), 1, "a turn is already active"));
                } else {
                    io.write(FakeZbrtServer.errorFrame(exec.requestId(), 1, "duplicate request id"));
                }
            }
        })) {
            assertEquals(0, conn.execute(List.of("x"), null, new byte[0], 0).code());
            RemoteError second = assertThrows(RemoteError.class,
                    () -> conn.execute(List.of("x"), null, new byte[0], 0));
            assertTrue(second.getMessage().contains("a turn is already active"));
            RemoteError third = assertThrows(RemoteError.class,
                    () -> conn.execute(List.of("x"), null, new byte[0], 0));
            assertTrue(third.getMessage().contains("duplicate request id"));
        }
    }

    @Test
    void cancelWithoutTargetExpectsEmptyCancelAck() throws Exception {
        try (ZbrtConnection conn = open(io -> {
            ZbrtFrame cancel = io.read();
            assertEquals(ZbrtFrame.KIND_CANCEL, cancel.kind());
            ZbrtCodec.Cancel decoded = ZbrtCodec.decodeCancel(cancel.payload());
            assertEquals("user", decoded.reason());
            io.write(io.reply(cancel, ZbrtFrame.KIND_CANCEL_ACK, new byte[0]));
        })) {
            conn.cancel("user", null); // must return without raising
        }
    }

    @Test
    void cancelWithTargetRoundTripsTarget() throws Exception {
        try (ZbrtConnection conn = open(io -> {
            ZbrtFrame cancel = io.read();
            ZbrtCodec.Cancel decoded = ZbrtCodec.decodeCancel(cancel.payload());
            byte[] target = decoded.target();
            if (target == null) {
                io.write(FakeZbrtServer.errorFrame(cancel.requestId(), 1, "missing target"));
                return;
            }
            io.write(io.reply(cancel, ZbrtFrame.KIND_CANCEL_ACK, new byte[0]));
        })) {
            conn.cancel(null, new byte[]{1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16});
        }
    }

    @Test
    void healthReturnsReady() throws Exception {
        try (ZbrtConnection conn = open(io -> {
            ZbrtFrame health = io.read();
            assertEquals(ZbrtFrame.KIND_HEALTH, health.kind());
            io.write(io.reply(health, ZbrtFrame.KIND_HEALTH_ACK, ZbrtCodec.encodeHealth(true, "ready")));
        })) {
            ZbrtCodec.Health health = conn.health();
            assertTrue(health.healthy());
            assertEquals("ready", health.message());
        }
    }

    @Test
    void fsReadAndWriteRoundTrip() throws Exception {
        try (ZbrtConnection conn = open(io -> {
            ZbrtFrame fsRead = io.read();
            assertEquals(ZbrtFrame.KIND_FS, fsRead.kind());
            ZbrtCodec.Fs read = ZbrtCodec.decodeFs(fsRead.payload());
            assertEquals(4, read.op());
            assertEquals("notes.txt", read.path());
            io.write(io.reply(fsRead, ZbrtFrame.KIND_FS_RESULT,
                    "{\"data\":[104,105],\"truncated\":false,\"total_bytes\":2}"
                            .getBytes(StandardCharsets.UTF_8)));

            ZbrtFrame fsWrite = io.read();
            ZbrtCodec.Fs write = ZbrtCodec.decodeFs(fsWrite.payload());
            assertEquals(5, write.op());
            assertTrue(new String(write.data(), StandardCharsets.UTF_8).contains("\"data\":[104,105]"));
            io.write(io.reply(fsWrite, ZbrtFrame.KIND_FS_RESULT,
                    "{\"bytes_written\":2}".getBytes(StandardCharsets.UTF_8)));
        })) {
            byte[] readPayload = conn.fs(4, "notes.txt",
                    "{\"max_bytes\":51200}".getBytes(StandardCharsets.UTF_8));
            assertTrue(new String(readPayload, StandardCharsets.UTF_8).contains("\"data\":[104,105]"));
            byte[] writePayload = conn.fs(5, "notes.txt",
                    "{\"data\":[104,105],\"append\":false}".getBytes(StandardCharsets.UTF_8));
            assertTrue(new String(writePayload, StandardCharsets.UTF_8).contains("bytes_written"));
        }
    }

    @Test
    void executeReadStallRaisesTransportError() throws Exception {
        // Timeout is a transport-level failure: the client never cancels on
        // its own (mirrors the Rust baseline).
        server = new FakeZbrtServer(io -> {
            io.read(); // consume the Execute, then stay silent
            try {
                Thread.sleep(1000);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        });
        try (ZbrtConnection conn = new ZbrtConnection(
                io.rfb.sdk.internal.GuestNdjson.parseAddress(server.address()),
                java.time.Duration.ofMillis(300))) {
            long start = System.nanoTime();
            assertThrows(TransportError.class,
                    () -> conn.execute(List.of("sleep"), null, new byte[0], 300));
            assertTrue((System.nanoTime() - start) < 3_000_000_000L, "timeout path must stay fast");
        }
    }

    @Test
    void requestIdMismatchRaisesDecodeError() throws Exception {
        try (ZbrtConnection conn = open(io -> {
            ZbrtFrame exec = io.read();
            byte[] otherId = new byte[16];
            Arrays.fill(otherId, (byte) 0xAB);
            io.write(FakeZbrtServer.exitFrame(otherId, 0));
        })) {
            assertThrows(DecodeError.class, () -> conn.execute(List.of("x"), null, new byte[0], 0));
        }
    }

    @Test
    void streamSessionYieldsOutputEventsThenTerminalAndStopCancels() throws Exception {
        try (ZbrtConnection conn = open(io -> {
            ZbrtFrame exec = io.read();
            assertEquals(ZbrtFrame.KIND_EXECUTE, exec.kind());
            io.write(FakeZbrtServer.outputFrame(exec.requestId(), 0, "line".getBytes(StandardCharsets.UTF_8)));
            ZbrtFrame cancel = io.read();
            assertEquals(ZbrtFrame.KIND_CANCEL, cancel.kind());
            ZbrtCodec.Cancel decoded = ZbrtCodec.decodeCancel(cancel.payload());
            assertArrayEquals(exec.requestId(), decoded.target());
            io.write(io.reply(cancel, ZbrtFrame.KIND_CANCEL_ACK, new byte[0]));
            io.write(FakeZbrtServer.exitFrame(exec.requestId(), -1));
        })) {
            ZbrtConnection.ZbrtStreamSession session = conn.openStreamSession(
                    List.of("tail", "-f"), null, new byte[0], 0);
            ZbrtConnection.Event chunk = session.nextEvent();
            assertEquals(0, chunk.stream());
            assertEquals("line", new String(chunk.data(), StandardCharsets.UTF_8));
            session.stop(); // sends Cancel targeted at this request; expects CancelAck
            ZbrtConnection.Event exit = session.nextEvent();
            assertTrue(exit.isExit());
            assertEquals(Integer.valueOf(-1), exit.code());
        }
    }

    // ---- Sandbox facade over the ZBRT transport ------------------------------

    @Test
    void sandboxExecOverZbrtMatchesNdjsonShape() throws Exception {
        server = new FakeZbrtServer(io -> {
            ZbrtFrame exec = io.read();
            io.writeFrames(
                    FakeZbrtServer.outputFrame(exec.requestId(), 0, "hi".getBytes(StandardCharsets.UTF_8)),
                    FakeZbrtServer.exitFrame(exec.requestId(), 0));
        });
        RfbClient client = new RfbClient("http://127.0.0.1:1", "", 5.0);
        SandboxInfo info = new SandboxInfo();
        info.setId("sb-1");
        info.setGuestAddr(server.address());
        Sandbox sandbox = Sandbox.attach(client, info, RfbClient.TRANSPORT_ZBRT);
        ExecResult result = sandbox.exec(List.of("echo", "hi"), "/workspace", 60.0);
        assertEquals(Integer.valueOf(0), result.getExitCode());
        assertEquals("hi", result.stdoutText());
        assertEquals("", result.stderrText());
        assertEquals(Boolean.FALSE, Boolean.valueOf(result.isTimedOut()));
    }

    @Test
    void sandboxPingOverZbrtUsesHealth() throws Exception {
        server = new FakeZbrtServer(io -> {
            ZbrtFrame health = io.read();
            assertEquals(ZbrtFrame.KIND_HEALTH, health.kind());
            io.write(io.reply(health, ZbrtFrame.KIND_HEALTH_ACK, ZbrtCodec.encodeHealth(true, "ready")));
        });
        RfbClient client = new RfbClient("http://127.0.0.1:1", "", 5.0);
        SandboxInfo info = new SandboxInfo();
        info.setId("sb-1");
        info.setGuestAddr(server.address());
        Sandbox sandbox = Sandbox.attach(client, info, RfbClient.TRANSPORT_ZBRT);
        assertTrue(sandbox.ping());
    }

    @Test
    void execValidatesArgsAndCwdBeforeSending() throws Exception {
        server = new FakeZbrtServer(io -> io.read());
        RfbClient client = new RfbClient("http://127.0.0.1:1", "", 5.0);
        SandboxInfo info = new SandboxInfo();
        info.setId("sb-1");
        info.setGuestAddr(server.address());
        Sandbox zbrt = Sandbox.attach(client, info, RfbClient.TRANSPORT_ZBRT);
        Sandbox ndjson = Sandbox.attach(client, info, RfbClient.TRANSPORT_NDJSON);
        // fail closed on both transports: empty argv and bad cwd never reach the wire
        assertThrows(ValidationError.class, () -> zbrt.exec(List.of(), "/workspace", 60.0));
        assertThrows(ValidationError.class, () -> zbrt.exec(List.of("x"), "a/../b", 60.0));
        assertThrows(ValidationError.class, () -> ndjson.exec(List.of(), "/workspace", 60.0));
        assertThrows(ValidationError.class, () -> ndjson.exec(List.of("x"), "C:/tmp", 60.0));
    }

    @Test
    void evalOverZbrtRejectsBlankCodeBeforeConnect() {
        RfbClient client = new RfbClient("http://127.0.0.1:1", "", 5.0);
        SandboxInfo info = new SandboxInfo();
        info.setId("sb-1");
        info.setGuestAddr("127.0.0.1:1"); // nothing is listening; nothing may be sent
        Sandbox sandbox = Sandbox.attach(client, info, RfbClient.TRANSPORT_ZBRT);
        assertThrows(ValidationError.class, () -> sandbox.eval("   "));
    }
}
