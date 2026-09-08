package io.rfb.sdk;

import io.rfb.sdk.internal.ZbrtCodec;
import io.rfb.sdk.internal.ZbrtFrame;
import org.junit.jupiter.api.Test;

import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;
import java.util.List;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;

/**
 * PROTOCOL.md §4 golden vectors: every vector must hit BOTH directions —
 * encode matches the reference hex, and decoding the reference hex yields the
 * expected structure. Plus strict-decode rejection cases (§3.1).
 */
class ZbrtFrameCodecTest {
    private static final String RID_HEX = "000102030405060708090a0b0c0d0e0f";

    private static byte[] rid() {
        return hex(RID_HEX);
    }

    static byte[] hex(String s) {
        byte[] out = new byte[s.length() / 2];
        for (int i = 0; i < out.length; i++) {
            out[i] = (byte) Integer.parseInt(s.substring(i * 2, i * 2 + 2), 16);
        }
        return out;
    }

    static String hexOf(byte[] bytes) {
        StringBuilder sb = new StringBuilder(bytes.length * 2);
        for (byte b : bytes) {
            sb.append(String.format("%02x", b));
        }
        return sb.toString();
    }

    /** magic(4) + version(1) + kind(1) + flags(2) + rid(16) + len(4) + payload */
    private static String vector(String kind, String len, String payloadHex) {
        return "5a425254" + "01" + kind + "0000" + RID_HEX + len + payloadHex;
    }

    // ---- encode matches golden hex ----------------------------------------

    @Test
    void encodeHelloMatchesGoldenVector() {
        byte[] frame = new ZbrtFrame(1, 0, rid(), ZbrtCodec.encodeHello(
                "sdk-test", List.of("execute", "stream"))).encode();
        assertEquals(vector("01", "00000022",
                "0000000873646b2d746573740200000007657865637574650000000673747265616d"), hexOf(frame));
    }

    @Test
    void encodeHelloAckMatchesGoldenVector() {
        byte[] frame = new ZbrtFrame(2, 0, rid(), ZbrtCodec.encodeHelloAck("rfb-zeroboot-guest",
                List.of("execute", "stream", "deadline", "health", "cancel", "filesystem"))).encode();
        assertEquals(vector("02", "0000005a",
                "000000127266622d7a65726f626f6f742d67756573740600000007657865637574650000000673747265616d"
                        + "00000008646561646c696e65000000066865616c74680000000663616e63656c0000000a66696c6573797374656d"),
                hexOf(frame));
    }

    @Test
    void encodeExecuteMatchesGoldenVector() {
        byte[] frame = new ZbrtFrame(3, 0, rid(), ZbrtCodec.encodeExecute(
                List.of("echo", "hi"), "/workspace",
                "abc".getBytes(StandardCharsets.UTF_8), 1500)).encode();
        assertEquals(vector("03", "00000029",
                "02000000046563686f000000026869010000000a2f776f726b737061636500000003616263000005dc"),
                hexOf(frame));
    }

    @Test
    void encodeOutputMatchesGoldenVector() {
        byte[] frame = new ZbrtFrame(4, 0, rid(), ZbrtCodec.encodeOutput(1,
                "err line\n".getBytes(StandardCharsets.UTF_8))).encode();
        assertEquals(vector("04", "0000000e", "0100000009657272206c696e650a"), hexOf(frame));
    }

    @Test
    void encodeExitMatchesGoldenVector() {
        byte[] frame = new ZbrtFrame(5, 0, rid(), ZbrtCodec.encodeExit(0, null)).encode();
        assertEquals(vector("05", "00000005", "0000000000"), hexOf(frame));
    }

    @Test
    void encodeCancelMatchesGoldenVector() {
        byte[] frame = new ZbrtFrame(6, 0, rid(),
                ZbrtCodec.encodeCancel("user", null)).encode();
        assertEquals(vector("06", "0000000a", "01000000047573657200"), hexOf(frame));
    }

    @Test
    void encodeLegacyCancelMatchesGoldenVector() {
        // Legacy form: reason flag + reason, NO target byte at all.
        byte[] payload = hex("010000000475736572");
        byte[] frame = new ZbrtFrame(6, 0, rid(), payload).encode();
        assertEquals(vector("06", "00000009", "010000000475736572"), hexOf(frame));
    }

    @Test
    void encodeErrorMatchesGoldenVector() {
        byte[] frame = new ZbrtFrame(12, 0, rid(),
                ZbrtCodec.encodeError(1, "argv is empty")).encode();
        assertEquals(vector("0c", "00000015", "000000010000000d6172677620697320656d707479"), hexOf(frame));
    }

    // ---- decode golden hex matches struct ---------------------------------

    @Test
    void decodeGoldenVectorsMatchStructs() {
        ZbrtFrame hello = ZbrtFrame.decode(hex(vector("01", "00000022",
                "0000000873646b2d746573740200000007657865637574650000000673747265616d")));
        assertEquals(1, hello.kind());
        assertEquals(0, hello.flags());
        assertArrayEquals(rid(), hello.requestId());
        ZbrtFrame decoded = ZbrtFrame.decode(new ByteArrayInputStream(hello.encode()));
        assertEquals(1, decoded.kind());

        ZbrtFrame helloAck = ZbrtFrame.decode(hex(vector("02", "0000005a",
                "000000127266622d7a65726f626f6f742d67756573740600000007657865637574650000000673747265616d"
                        + "00000008646561646c696e65000000066865616c74680000000663616e63656c0000000a66696c6573797374656d")));
        ZbrtCodec.HelloAck ack = ZbrtCodec.decodeHelloAck(helloAck.payload());
        assertEquals("rfb-zeroboot-guest", ack.server);
        assertEquals(List.of("execute", "stream", "deadline", "health", "cancel", "filesystem"),
                ack.capabilities);

        ZbrtFrame execute = ZbrtFrame.decode(hex(vector("03", "00000029",
                "02000000046563686f000000026869010000000a2f776f726b737061636500000003616263000005dc")));
        ZbrtCodec.Execute exec = ZbrtCodec.decodeExecute(execute.payload());
        assertEquals(List.of("echo", "hi"), exec.argv());
        assertEquals("/workspace", exec.cwd());
        assertEquals("abc", new String(exec.stdin(), StandardCharsets.UTF_8));
        assertEquals(1500, exec.timeoutMs());

        ZbrtFrame output = ZbrtFrame.decode(hex(vector("04", "0000000e", "0100000009657272206c696e650a")));
        ZbrtCodec.Output out = ZbrtCodec.decodeOutput(output.payload());
        assertEquals(1, out.stream());
        assertEquals("err line\n", new String(out.data(), StandardCharsets.UTF_8));

        ZbrtFrame exit = ZbrtFrame.decode(hex(vector("05", "00000005", "0000000000")));
        ZbrtCodec.Exit exitPayload = ZbrtCodec.decodeExit(exit.payload());
        assertEquals(0, exitPayload.code());
        assertNull(exitPayload.signal());

        ZbrtFrame legacy = ZbrtFrame.decode(hex(vector("06", "00000009", "010000000475736572")));
        ZbrtCodec.Cancel legacyCancel = ZbrtCodec.decodeCancel(legacy.payload());
        assertEquals("user", legacyCancel.reason());
        assertNull(legacyCancel.target());

        ZbrtFrame modern = ZbrtFrame.decode(hex(vector("06", "0000000a", "01000000047573657200")));
        ZbrtCodec.Cancel modernCancel = ZbrtCodec.decodeCancel(modern.payload());
        assertEquals("user", modernCancel.reason());
        assertNull(modernCancel.target());

        ZbrtFrame error = ZbrtFrame.decode(hex(vector("0c", "00000015",
                "000000010000000d6172677620697320656d707479")));
        ZbrtCodec.ZbrtErrorPayload errorPayload = ZbrtCodec.decodeError(error.payload());
        assertEquals(1, errorPayload.code());
        assertEquals("argv is empty", errorPayload.message());
    }

    // ---- strict-decode rejections -----------------------------------------

    @Test
    void rejectsBadMagic() {
        byte[] frame = new ZbrtFrame(5, 0, rid(), new byte[5]).encode();
        frame[0] = 'X';
        assertThrows(DecodeError.class, () -> ZbrtFrame.decode(frame));
    }

    @Test
    void rejectsWrongVersion() {
        byte[] frame = new ZbrtFrame(5, 0, rid(), new byte[5]).encode();
        frame[4] = 2;
        assertThrows(DecodeError.class, () -> ZbrtFrame.decode(frame));
    }

    @Test
    void rejectsNonZeroFlags() {
        assertThrows(DecodeError.class,
                () -> new ZbrtFrame(5, 0x0001, rid(), new byte[5]).encode());
        byte[] raw = new ZbrtFrame(5, 0, rid(), new byte[5]).encode();
        raw[7] = 1; // flags low byte = 1
        assertThrows(DecodeError.class, () -> ZbrtFrame.decode(raw));
    }

    @Test
    void rejectsUnknownKind() {
        assertThrows(DecodeError.class, () -> new ZbrtFrame(14, 0, rid(), new byte[0]));
        byte[] raw = new ZbrtFrame(5, 0, rid(), new byte[5]).encode();
        raw[5] = 0; // kind 0 unknown
        assertThrows(DecodeError.class, () -> ZbrtFrame.decode(raw));
    }

    @Test
    void rejectsTruncatedHeader() {
        byte[] raw = new ZbrtFrame(5, 0, rid(), new byte[5]).encode();
        assertThrows(DecodeError.class,
                () -> ZbrtFrame.decode(new ByteArrayInputStream(raw, 0, raw.length - 1)));
    }

    @Test
    void rejectsTruncatedPayload() {
        byte[] raw = new ZbrtFrame(5, 0, rid(), new byte[5]).encode();
        byte[] cut = new byte[28 + 3];
        System.arraycopy(raw, 0, cut, 0, cut.length);
        assertThrows(DecodeError.class, () -> ZbrtFrame.decode(cut));
        assertThrows(DecodeError.class,
                () -> ZbrtFrame.decode(new ByteArrayInputStream(cut)));
    }

    @Test
    void rejectsOversizePayloadLength() {
        byte[] raw = new ZbrtFrame(5, 0, rid(), new byte[0]).encode();
        raw[24] = (byte) 0x02; // 0x02000000 > 16 MiB
        assertThrows(DecodeError.class, () -> ZbrtFrame.decode(raw));
    }

    @Test
    void rejectsTrailingPayloadBytesInStrictPayloadCodecs() {
        byte[] exit = ZbrtCodec.encodeExit(0, null);
        byte[] withTrailing = new byte[exit.length + 1];
        System.arraycopy(exit, 0, withTrailing, 0, exit.length);
        withTrailing[exit.length] = 0;
        assertThrows(DecodeError.class, () -> ZbrtCodec.decodeExit(withTrailing));

        byte[] helloAck = ZbrtCodec.encodeHelloAck("srv", List.of("execute"));
        byte[] ackTrailing = new byte[helloAck.length + 2];
        System.arraycopy(helloAck, 0, ackTrailing, 0, helloAck.length);
        assertThrows(DecodeError.class, () -> ZbrtCodec.decodeHelloAck(ackTrailing));

        byte[] error = ZbrtCodec.encodeError(1, "x");
        byte[] errorTrailing = new byte[error.length + 1];
        System.arraycopy(error, 0, errorTrailing, 0, error.length);
        assertThrows(DecodeError.class, () -> ZbrtCodec.decodeError(errorTrailing));
    }

    @Test
    void rejectsTruncatedStrictPayloads() {
        byte[] exit = ZbrtCodec.encodeExit(0, null);
        assertThrows(DecodeError.class, () -> ZbrtCodec.decodeExit(new byte[exit.length - 1]));
        byte[] cancel = ZbrtCodec.encodeCancel("reason", rid());
        byte[] cutTarget = new byte[cancel.length - 8];
        System.arraycopy(cancel, 0, cutTarget, 0, cutTarget.length);
        assertThrows(DecodeError.class, () -> ZbrtCodec.decodeCancel(cutTarget));
    }

    @Test
    void decodesCancelWithTarget() {
        byte[] payload = ZbrtCodec.encodeCancel("user", rid());
        ZbrtCodec.Cancel cancel = ZbrtCodec.decodeCancel(payload);
        assertEquals("user", cancel.reason());
        assertArrayEquals(rid(), cancel.target());
    }

    @Test
    void decodesFramesFromStreamIncrementally() {
        byte[] f1 = new ZbrtFrame(4, 0, rid(), ZbrtCodec.encodeOutput(0, "a".getBytes())).encode();
        byte[] f2 = new ZbrtFrame(5, 0, rid(), ZbrtCodec.encodeExit(7, null)).encode();
        byte[] both = new byte[f1.length + f2.length];
        System.arraycopy(f1, 0, both, 0, f1.length);
        System.arraycopy(f2, 0, both, f1.length, f2.length);
        ByteArrayInputStream in = new ByteArrayInputStream(both);
        assertEquals(4, ZbrtFrame.decode(in).kind());
        assertEquals(5, ZbrtFrame.decode(in).kind());
    }
}
