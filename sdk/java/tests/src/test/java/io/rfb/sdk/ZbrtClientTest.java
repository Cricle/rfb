package io.rfb.sdk;

import io.rfb.sdk.internal.ZbrtFrame;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.Test;

import java.util.concurrent.atomic.AtomicInteger;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

/**
 * ZBRT fail-closed contract for options the wire cannot express.
 *
 * <p>{@code Sandbox.eval} over ZBRT fails closed: ZBRT v1 has no eval opcode
 * and the reference guest maps Execute verbatim onto {@code exec}, so the old
 * "facade convention" (argv=["eval", code]) surfaced the guest's
 * {@code eval: not found} exit code as a successful result. The facade now
 * rejects eval over ZBRT locally, with zero frames on the wire — mirroring the
 * Rust baseline.
 */
class ZbrtClientTest {

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

    @Test
    void evalOverZbrtFailsClosedWithoutSendingFrames() throws Exception {
        AtomicInteger frames = new AtomicInteger(0);
        server = new FakeZbrtServer(io -> {
            frames.incrementAndGet();
            ZbrtFrame exec = io.read();
            io.write(FakeZbrtServer.exitFrame(exec.requestId(), 0));
        });
        Sandbox sandbox = zbrtSandbox();
        assertThrows(ValidationError.class, () -> sandbox.eval("1+1", "/workspace", 5.0));
        assertThrows(ValidationError.class, () -> sandbox.eval("print(40+2)"));

        // Local validation still runs first and also stays off the wire.
        assertThrows(ValidationError.class, () -> sandbox.eval("   "), "blank code");
        assertThrows(ValidationError.class, () -> sandbox.eval("1", null, 0.0), "timeout_s=0");
        assertEquals(0, frames.get(), "no frames sent for any rejected eval");
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
        assertThrows(ValidationError.class, () -> sandbox.stream(java.util.List.of("sh"), null, true, null));
        assertThrows(ValidationError.class,
                () -> sandbox.stream(java.util.List.of("sh"), null, null, java.util.Map.of("A", "1")));
        assertThrows(ValidationError.class, () -> sandbox.stream(java.util.List.of(), null, null, null));
        assertEquals(0, frames.get(), "unsupported stream options must not touch the wire");
    }
}
