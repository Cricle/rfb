package io.rfb.sdk;

import com.fasterxml.jackson.databind.JsonNode;
import io.rfb.sdk.internal.Json;
import io.rfb.sdk.internal.ZbrtCodec;
import io.rfb.sdk.internal.ZbrtFrame;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.Test;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.HexFormat;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;

/**
 * Conformance vectors from {@code sdk/shared/conformance/eval_zbrt_vectors.json}:
 * {@code Sandbox.eval} over the ZBRT transport encodes one Execute turn with
 * {@code argv=["eval", code]} and must match the golden frame bytes. The
 * client's random per-request id is normalized to the shared golden id before
 * comparing wire bytes (same approach as the Rust baseline test).
 */
class ZbrtClientTest {
    /** Shared conformance vectors: sdk/shared/conformance/eval_zbrt_vectors.json. */
    private static final JsonNode SHARED_VECTORS = loadSharedVectors();

    private static JsonNode loadSharedVectors() {
        Path current = Paths.get("").toAbsolutePath();
        while (current != null) {
            Path candidate = current.resolve("sdk/shared/conformance/eval_zbrt_vectors.json");
            if (Files.isRegularFile(candidate)) {
                try {
                    return Json.MAPPER.readTree(candidate.toFile());
                } catch (IOException e) {
                    throw new IllegalStateException("cannot read " + candidate, e);
                }
            }
            current = current.getParent();
        }
        throw new IllegalStateException(
                "shared conformance vectors not found from " + Paths.get("").toAbsolutePath());
    }

    /** Golden frame hex for a named shared vector. */
    private static String sharedVectorHex(String name) {
        for (JsonNode vector : SHARED_VECTORS.get("vectors")) {
            if (name.equals(vector.path("name").asText())) {
                return vector.path("expected_frame_hex").asText();
            }
        }
        throw new IllegalStateException("missing shared vector: " + name);
    }

    private static final String GOLDEN_REQUEST_ID_HEX =
            SHARED_VECTORS.path("request_id_hex").asText();

    private FakeZbrtServer server;

    @AfterEach
    void tearDown() {
        if (server != null) {
            server.close();
        }
    }

    private Sandbox zbrtSandbox() {
        RfbClient client = new RfbClient("http://127.0.0.1:1", "", 5.0);
        SandboxInfo info = new SandboxInfo();
        info.setId("sb-1");
        info.setGuestAddr(server.address());
        return Sandbox.attach(client, info, RfbClient.TRANSPORT_ZBRT);
    }

    /** Re-encode a frame with its request id normalized to the shared golden id. */
    private static String normalizedFrameHex(ZbrtFrame frame) {
        byte[] bytes = frame.encode();
        System.arraycopy(HexFormat.of().parseHex(GOLDEN_REQUEST_ID_HEX), 0, bytes, 8, 16);
        return HexFormat.of().formatHex(bytes);
    }

    @Test
    void evalZbrtMatchesSharedVector() throws Exception {
        // EVAL_ZBRT_BASIC: eval("1+1", cwd="/workspace", timeout_s=5) →
        // Execute(argv=["eval","1+1"], cwd flag 1, stdin empty, timeout_ms=5000).
        AtomicReference<ZbrtFrame> captured = new AtomicReference<>();
        server = new FakeZbrtServer(io -> {
            ZbrtFrame exec = io.read();
            captured.set(exec);
            io.writeFrames(
                    FakeZbrtServer.outputFrame(exec.requestId(), 0, HexFormat.of().parseHex("32")),
                    FakeZbrtServer.exitFrame(exec.requestId(), 0));
        });
        ExecResult result = zbrtSandbox().eval("1+1", "/workspace", 5.0);
        assertEquals(
                sharedVectorHex("EVAL_ZBRT_BASIC"),
                normalizedFrameHex(captured.get()),
                "EVAL_ZBRT_BASIC wire bytes");
        assertExecuteFields(captured.get(), List.of("eval", "1+1"), "/workspace", 5000);
        assertEquals(Integer.valueOf(0), result.getExitCode());
        assertArrayEquals(HexFormat.of().parseHex("32"), result.getStdout());
        assertEquals(0, result.getStderr().length);
        assertFalse(result.isTimedOut());
        server.close();

        // EVAL_ZBRT_DEFAULTS: eval("print(40+2)") — cwd flag 0, timeout_ms=0.
        AtomicReference<ZbrtFrame> capturedDefaults = new AtomicReference<>();
        server = new FakeZbrtServer(io -> {
            ZbrtFrame exec = io.read();
            capturedDefaults.set(exec);
            io.writeFrames(
                    FakeZbrtServer.outputFrame(exec.requestId(), 0, HexFormat.of().parseHex("3432")),
                    FakeZbrtServer.exitFrame(exec.requestId(), 0));
        });
        ExecResult defaults = zbrtSandbox().eval("print(40+2)");
        assertEquals(
                sharedVectorHex("EVAL_ZBRT_DEFAULTS"),
                normalizedFrameHex(capturedDefaults.get()),
                "EVAL_ZBRT_DEFAULTS wire bytes");
        assertExecuteFields(capturedDefaults.get(), List.of("eval", "print(40+2)"), null, 0);
        assertEquals(Integer.valueOf(0), defaults.getExitCode());
        assertArrayEquals(HexFormat.of().parseHex("3432"), defaults.getStdout());
    }

    /** Field-level assertions on a captured Execute frame (shared vector contract). */
    private static void assertExecuteFields(ZbrtFrame frame, List<String> argv,
                                            String cwd, long timeoutMs) {
        assertEquals(ZbrtFrame.KIND_EXECUTE, frame.kind());
        // The client's per-request id is random; the golden-id contract on the
        // wire is already asserted by the normalizedFrameHex comparisons above.
        ZbrtCodec.Execute exec = ZbrtCodec.decodeExecute(frame.payload());
        assertEquals(argv.size(), exec.argv().size(), "argc");
        assertEquals(argv, exec.argv(), "argv");
        if (cwd == null) {
            assertNull(exec.cwd(), "cwd flag must be 0 (absent)");
        } else {
            assertEquals(cwd, exec.cwd(), "cwd");
        }
        assertEquals(0, exec.stdin().length, "stdin must be empty");
        assertEquals(timeoutMs, exec.timeoutMs(), "timeout_ms");
    }

    @Test
    void evalZbrtValidationRejectedSendsNoFrames() throws Exception {
        // EVAL_ZBRT_VALIDATION_REJECTED + EVAL_ZBRT_TIMEOUT_ZERO_REJECTED:
        // both fail closed locally with zero frames on the wire.
        AtomicInteger frames = new AtomicInteger(0);
        server = new FakeZbrtServer(io -> {
            frames.incrementAndGet();
            ZbrtFrame exec = io.read();
            io.write(FakeZbrtServer.exitFrame(exec.requestId(), 0));
        });
        Sandbox sandbox = zbrtSandbox();
        assertThrows(ValidationError.class, () -> sandbox.eval("   "), "blank code");
        assertThrows(ValidationError.class, () -> sandbox.eval("1", null, 0.0), "timeout_s=0");
        assertEquals(0, frames.get(), "validation failures must not touch the wire");
    }

    @Test
    void streamZbrtRejectsPtyEnvAndEmptyArgvSendsNoFrames() throws Exception {
        // Fail-closed parity with the Rust/C#/Python baselines: pty and env
        // are ZBRT-unsupported options that must raise ValidationError before
        // any frame is sent (not be silently ignored), and empty argv is
        // rejected on every transport.
        AtomicInteger frames = new AtomicInteger(0);
        server = new FakeZbrtServer(io -> {
            frames.incrementAndGet();
            ZbrtFrame exec = io.read();
            io.write(FakeZbrtServer.exitFrame(exec.requestId(), 0));
        });
        Sandbox sandbox = zbrtSandbox();
        assertThrows(ValidationError.class,
                () -> sandbox.stream(List.of("cat"), null, true, null), "pty over zbrt");
        assertThrows(ValidationError.class,
                () -> sandbox.stream(List.of("cat"), null, null, Map.of("K", "V")), "env over zbrt");
        assertThrows(ValidationError.class,
                () -> sandbox.stream(List.of(), null, null, null), "empty argv over zbrt");
        assertEquals(0, frames.get(), "rejections must not touch the wire");
    }
}
