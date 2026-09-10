package io.rfb.sdk;

import io.rfb.sdk.internal.ZbrtCodec;
import io.rfb.sdk.internal.ZbrtFrame;
import org.junit.jupiter.api.Test;

import java.nio.charset.StandardCharsets;
import java.util.List;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/** Strict payload codec round-trips (PROTOCOL.md §3.2). */
class ZbrtCodecTest {
    private static final byte[] RID = ZbrtFrameCodecTest.hex("000102030405060708090a0b0c0d0e0f");

    @Test
    void executeRoundTripWithoutCwd() {
        byte[] payload = ZbrtCodec.encodeExecute(List.of("run", "x"), null,
                new byte[]{1, 2, 3}, 0);
        ZbrtCodec.Execute exec = ZbrtCodec.decodeExecute(payload);
        assertEquals(List.of("run", "x"), exec.argv());
        assertNull(exec.cwd());
        assertArrayEquals(new byte[]{1, 2, 3}, exec.stdin());
        assertEquals(0, exec.timeoutMs());
        // argc(1) + "run"(4+3) + "x"(4+1) + cwd flag(1) + stdin prefix(4)
        // + stdin(3) + timeout(4) → total 25.
        assertEquals(25, payload.length);
        assertEquals(0, payload[13]); // no-cwd flag byte is 0
    }

    @Test
    void executeRoundTripWithCwdAndTimeout() {
        ZbrtCodec.Execute exec = ZbrtCodec.decodeExecute(ZbrtCodec.encodeExecute(
                List.of("echo", "hi"), "/workspace",
                "abc".getBytes(StandardCharsets.UTF_8), 1500));
        assertEquals(List.of("echo", "hi"), exec.argv());
        assertEquals("/workspace", exec.cwd());
        assertEquals("abc", new String(exec.stdin(), StandardCharsets.UTF_8));
        assertEquals(1500, exec.timeoutMs());
    }

    @Test
    void executeRejectsTooManyArguments() {
        String[] big = new String[256];
        java.util.Arrays.fill(big, "a");
        assertThrows(DecodeError.class,
                () -> ZbrtCodec.encodeExecute(List.of(big), null, new byte[0], 0));
    }

    @Test
    void outputRoundTrip() {
        byte[] data = "out".getBytes(StandardCharsets.UTF_8);
        ZbrtCodec.Output out = ZbrtCodec.decodeOutput(ZbrtCodec.encodeOutput(0, data));
        assertEquals(0, out.stream());
        assertArrayEquals(data, out.data());
        ZbrtCodec.Output err = ZbrtCodec.decodeOutput(ZbrtCodec.encodeOutput(1, data));
        assertEquals(1, err.stream());
    }

    @Test
    void exitRoundTripWithSignal() {
        ZbrtCodec.Exit exit = ZbrtCodec.decodeExit(ZbrtCodec.encodeExit(-9, 15L));
        assertEquals(-9, exit.code());
        assertEquals(15L, exit.signal());
        ZbrtCodec.Exit plain = ZbrtCodec.decodeExit(ZbrtCodec.encodeExit(0, null));
        assertNull(plain.signal());
    }

    @Test
    void cancelLegacyHasNoTargetByte() {
        ZbrtCodec.Cancel cancel = ZbrtCodec.decodeCancel(new byte[]{
                1, 0, 0, 0, 4, 'u', 's', 'e', 'r'});
        assertEquals("user", cancel.reason());
        assertNull(cancel.target());
    }

    @Test
    void cancelModernNoTargetCarriesFlagZero() {
        byte[] modern = ZbrtCodec.encodeCancel("user", null);
        assertEquals(10, modern.length); // 1 + 4 + 4 + 1
        assertEquals(0, modern[9]);
        ZbrtCodec.Cancel cancel = ZbrtCodec.decodeCancel(modern);
        assertEquals("user", cancel.reason());
        assertNull(cancel.target());
    }

    @Test
    void cancelWithTargetRoundTrip() {
        ZbrtCodec.Cancel cancel = ZbrtCodec.decodeCancel(ZbrtCodec.encodeCancel(null, RID));
        assertNull(cancel.reason());
        assertArrayEquals(RID, cancel.target());
    }

    @Test
    void fsRoundTrip() {
        byte[] data = "{\"max_results\":1000}".getBytes(StandardCharsets.UTF_8);
        byte[] payload = ZbrtCodec.encodeFs(1, "/workspace", data);
        ZbrtCodec.Fs fs = ZbrtCodec.decodeFs(payload);
        assertEquals(1, fs.op());
        assertEquals("/workspace", fs.path());
        assertArrayEquals(data, fs.data());
    }

    @Test
    void healthRoundTrip() {
        ZbrtCodec.Health health = ZbrtCodec.decodeHealth(ZbrtCodec.encodeHealth(true, null));
        assertTrue(health.healthy());
        assertNull(health.message());
        ZbrtCodec.Health withMessage = ZbrtCodec.decodeHealth(ZbrtCodec.encodeHealth(false, "why"));
        assertFalse(withMessage.healthy());
        assertEquals("why", withMessage.message());
    }

    @Test
    void helloAckRoundTrip() {
        ZbrtCodec.HelloAck ack = ZbrtCodec.decodeHelloAck(ZbrtCodec.encodeHelloAck(
                "rfb-zeroboot-guest", io.rfb.sdk.internal.ZbrtConnection.V1_CAPABILITIES));
        assertEquals("rfb-zeroboot-guest", ack.server);
        assertEquals(io.rfb.sdk.internal.ZbrtConnection.V1_CAPABILITIES, ack.capabilities);
    }
}
