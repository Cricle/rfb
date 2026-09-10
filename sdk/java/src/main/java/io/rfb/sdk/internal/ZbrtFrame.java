package io.rfb.sdk.internal;

import io.rfb.sdk.DecodeError;
import io.rfb.sdk.TransportError;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;

/**
 * ZBRT v1 wire frame (PROTOCOL.md §3.1, mirroring
 * {@code rfb-runtime/src/zeroboot_protocol.rs}). Header: magic "ZBRT",
 * version 1, kind, flags (u16 BE, must be 0), request_id (16 bytes),
 * payload_len (u32 BE, &le; 16 MiB). INTERNAL.
 */
public record ZbrtFrame(int kind, int flags, byte[] requestId, byte[] payload) {
    public static final int HEADER_LEN = 28;
    public static final int MAX_PAYLOAD = 16 * 1024 * 1024;
    static final byte[] MAGIC = {'Z', 'B', 'R', 'T'};
    static final int VERSION = 1;

    public static final int KIND_HELLO = 1;
    public static final int KIND_HELLO_ACK = 2;
    public static final int KIND_EXECUTE = 3;
    public static final int KIND_OUTPUT = 4;
    public static final int KIND_EXIT = 5;
    public static final int KIND_CANCEL = 6;
    public static final int KIND_CANCEL_ACK = 7;
    public static final int KIND_FS = 8;
    public static final int KIND_FS_RESULT = 9;
    public static final int KIND_HEALTH = 10;
    public static final int KIND_HEALTH_ACK = 11;
    public static final int KIND_ERROR = 12;

    public ZbrtFrame {
        if (requestId == null || requestId.length != 16) {
            throw new IllegalArgumentException("request_id must be 16 bytes");
        }
        checkKind(kind);
    }

    private static int checkKind(int kind) {
        if (kind < 1 || kind > 13) {
            throw new DecodeError("unknown frame kind: " + kind);
        }
        return kind;
    }

    public byte[] encode() {
        if (flags != 0 || payload.length > MAX_PAYLOAD) {
            throw new DecodeError("invalid frame");
        }
        byte[] out = new byte[HEADER_LEN + payload.length];
        out[0] = MAGIC[0];
        out[1] = MAGIC[1];
        out[2] = MAGIC[2];
        out[3] = MAGIC[3];
        out[4] = VERSION;
        out[5] = (byte) kind;
        out[6] = (byte) ((flags >>> 8) & 0xFF);
        out[7] = (byte) (flags & 0xFF);
        System.arraycopy(requestId, 0, out, 8, 16);
        int len = payload.length;
        out[24] = (byte) ((len >>> 24) & 0xFF);
        out[25] = (byte) ((len >>> 16) & 0xFF);
        out[26] = (byte) ((len >>> 8) & 0xFF);
        out[27] = (byte) (len & 0xFF);
        System.arraycopy(payload, 0, out, HEADER_LEN, payload.length);
        return out;
    }

    public void writeTo(OutputStream out) {
        try {
            out.write(encode());
            out.flush();
        } catch (IOException e) {
            throw new TransportError("zbrt write failed: " + e.getMessage(), e);
        }
    }

    /** Read one complete frame from a stream; strict header validation. */
    public static ZbrtFrame decode(InputStream in) {
        byte[] header = readExactly(in, HEADER_LEN, "truncated frame header");
        return decodeHeaderAndPayload(header, readExactly(in, payloadLength(header), "truncated frame payload"));
    }

    /** Decode one frame from a byte buffer (header + payload must be complete). */
    public static ZbrtFrame decode(byte[] bytes) {
        if (bytes.length < HEADER_LEN) {
            throw new DecodeError("truncated frame header");
        }
        byte[] header = new byte[HEADER_LEN];
        System.arraycopy(bytes, 0, header, 0, HEADER_LEN);
        int len = payloadLength(header);
        if (bytes.length < HEADER_LEN + len) {
            throw new DecodeError("truncated frame payload");
        }
        byte[] payload = new byte[len];
        System.arraycopy(bytes, HEADER_LEN, payload, 0, len);
        return decodeHeaderAndPayload(header, payload);
    }

    private static ZbrtFrame decodeHeaderAndPayload(byte[] header, byte[] payload) {
        checkHeaderMagic(header);
        int kind = checkKind(header[5] & 0xFF);
        int flags = ((header[6] & 0xFF) << 8) | (header[7] & 0xFF);
        if (flags != 0) {
            throw new DecodeError("unsupported frame flags: " + flags);
        }
        byte[] requestId = new byte[16];
        System.arraycopy(header, 8, requestId, 0, 16);
        return new ZbrtFrame(kind, flags, requestId, payload);
    }

    private static int payloadLength(byte[] header) {
        long len = ((header[24] & 0xFFL) << 24) | ((header[25] & 0xFFL) << 16)
                | ((header[26] & 0xFFL) << 8) | (header[27] & 0xFFL);
        if (len > MAX_PAYLOAD) {
            throw new DecodeError("payload too large: " + len);
        }
        return (int) len;
    }

    private static void checkHeaderMagic(byte[] header) {
        if (header[0] != MAGIC[0] || header[1] != MAGIC[1]
                || header[2] != MAGIC[2] || header[3] != MAGIC[3] || header[4] != VERSION) {
            throw new DecodeError("invalid magic or version");
        }
    }

    static byte[] readExactly(InputStream in, int n, String eofMessage) {
        byte[] out = new byte[n];
        int off = 0;
        while (off < n) {
            int r;
            try {
                r = in.read(out, off, n - off);
            } catch (IOException e) {
                throw new TransportError("zbrt read failed: " + e.getMessage(), e);
            }
            if (r < 0) {
                throw new DecodeError(eofMessage);
            }
            off += r;
        }
        return out;
    }
}
