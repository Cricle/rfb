using Rfb.Sdk.Internal;
using Xunit;

namespace Rfb.Sdk.Tests;

/// <summary>Strict frame/payload decode rejection cases (PROTOCOL.md §3.1).</summary>
public class ZbrtCodecStrictTests
{
    private static readonly byte[] RequestId = Hex.ToBytes("000102030405060708090a0b0c0d0e0f");

    private static ZbrtFrame ValidFrame() => new()
    {
        Kind = ZbrtKind.Execute,
        RequestId = RequestId,
        Payload = ZbrtFrameCodec.EncodeExecute(new[] { "echo" }, null, [], 0),
    };

    [Fact]
    public void Decode_RejectsBadMagic()
    {
        var bytes = ZbrtFrameCodec.Encode(ValidFrame());
        bytes[0] = (byte)'X';
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.Decode(bytes));
    }

    [Fact]
    public void Decode_RejectsWrongVersion()
    {
        var bytes = ZbrtFrameCodec.Encode(ValidFrame());
        bytes[4] = 2;
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.Decode(bytes));
    }

    [Fact]
    public void Decode_RejectsNonzeroFlags()
    {
        var bytes = ZbrtFrameCodec.Encode(ValidFrame());
        bytes[7] = 1; // flags low byte
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.Decode(bytes));
    }

    [Fact]
    public void Encode_RejectsNonzeroFlags()
    {
        var frame = new ZbrtFrame
        {
            Kind = ZbrtKind.Execute,
            Flags = 1,
            RequestId = RequestId,
            Payload = [],
        };
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.Encode(frame));
    }

    [Fact]
    public void Decode_RejectsUnknownKind()
    {
        var bytes = ZbrtFrameCodec.Encode(ValidFrame());
        bytes[5] = 0x7f;
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.Decode(bytes));
    }

    [Fact]
    public void Decode_RejectsKindZero()
    {
        var bytes = ZbrtFrameCodec.Encode(ValidFrame());
        bytes[5] = 0;
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.Decode(bytes));
    }

    [Fact]
    public void Decode_RejectsTruncatedHeader()
    {
        var bytes = ZbrtFrameCodec.Encode(ValidFrame());
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.Decode(bytes[..20]));
    }

    [Fact]
    public void Decode_RejectsTruncatedPayload()
    {
        var bytes = ZbrtFrameCodec.Encode(ValidFrame());
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.Decode(bytes[..^3]));
    }

    [Fact]
    public void Decode_RejectsOversizePayloadLength()
    {
        // header claims > 16 MiB payload
        var header = new byte[28];
        ZbrtFrameCodec.Magic.CopyTo(header, 0);
        header[4] = 1;
        header[5] = (byte)ZbrtKind.Execute;
        RequestId.CopyTo(header, 8);
        System.Buffers.Binary.BinaryPrimitives.WriteUInt32BigEndian(header.AsSpan(24, 4), 16 * 1024 * 1024 + 1);
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.Decode(header));
    }

    // ---- strict payload codecs: trailing bytes / truncation ----------------

    private static byte[] WithTrailing(byte[] payload, byte extra = 0xff)
    {
        var bytes = new byte[payload.Length + 1];
        payload.CopyTo(bytes, 0);
        bytes[^1] = extra;
        return bytes;
    }

    [Fact]
    public void Hello_RejectsTrailingBytes()
    {
        var payload = ZbrtFrameCodec.EncodeHello("c", new[] { "x" });
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.DecodeHello(WithTrailing(payload)));
    }

    [Fact]
    public void Hello_RejectsTruncated()
    {
        var payload = ZbrtFrameCodec.EncodeHello("client", new[] { "x" });
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.DecodeHello(payload[..^2]));
    }

    [Fact]
    public void Hello_RejectsMissingCountByte()
    {
        // string only, count byte absent
        var payload = new List<byte>();
        ZbrtFrameCodec.PutString(payload, "sdk-test");
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.DecodeHello(payload.ToArray()));
    }

    [Fact]
    public void HelloAck_RejectsTrailingBytes()
    {
        var payload = ZbrtFrameCodec.EncodeHelloAck("s", new[] { "x" });
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.DecodeHelloAck(WithTrailing(payload)));
    }

    [Fact]
    public void Execute_RejectsTrailingBytes()
    {
        var payload = ZbrtFrameCodec.EncodeExecute(new[] { "echo" }, null, [], 0);
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.DecodeExecute(WithTrailing(payload)));
    }

    [Fact]
    public void Execute_RejectsTruncated()
    {
        var payload = ZbrtFrameCodec.EncodeExecute(new[] { "echo" }, "/w", [1, 2], 5);
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.DecodeExecute(payload[..^2]));
    }

    [Fact]
    public void Output_RejectsTrailingBytes()
    {
        var payload = ZbrtFrameCodec.EncodeOutput(0, [1, 2, 3]);
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.DecodeOutput(WithTrailing(payload)));
    }

    [Fact]
    public void Exit_RejectsTrailingBytes()
    {
        var payload = ZbrtFrameCodec.EncodeExit(0, null);
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.DecodeExit(WithTrailing(payload)));
    }

    [Fact]
    public void Exit_DecodesSignal()
    {
        var payload = ZbrtFrameCodec.EncodeExit(-1, 9);
        var (code, signal) = ZbrtFrameCodec.DecodeExit(payload);
        Assert.Equal(-1, code);
        Assert.Equal(9u, signal);
    }

    [Fact]
    public void Cancel_ModernRoundTrip_WithTarget()
    {
        var payload = ZbrtFrameCodec.EncodeCancel("stop", RequestId);
        var cancel = ZbrtFrameCodec.DecodeCancel(payload);
        Assert.Equal("stop", cancel.Reason);
        Assert.Equal(RequestId, cancel.Target);
    }

    [Fact]
    public void Cancel_NoReasonNoTarget()
    {
        var payload = ZbrtFrameCodec.EncodeCancel(null, null);
        var cancel = ZbrtFrameCodec.DecodeCancel(payload);
        Assert.Null(cancel.Reason);
        Assert.Null(cancel.Target);
    }

    [Fact]
    public void Fs_RejectsTrailingBytes()
    {
        var payload = ZbrtFrameCodec.EncodeFs(1, ".", [0x7b, 0x7d]);
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.DecodeFs(WithTrailing(payload)));
    }

    [Fact]
    public void Fs_RoundTrips()
    {
        var payload = ZbrtFrameCodec.EncodeFs(4, "/workspace/a", "{}"u8.ToArray());
        var (op, path, data) = ZbrtFrameCodec.DecodeFs(payload);
        Assert.Equal(4, op);
        Assert.Equal("/workspace/a", path);
        Assert.Equal("{}"u8.ToArray(), data);
    }

    [Fact]
    public void Health_RejectsTruncatedMessageFlag()
    {
        var payload = ZbrtFrameCodec.EncodeHealth(true, "ready");
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.DecodeHealth(payload[..^3]));
    }

    [Fact]
    public void Error_RejectsTrailingBytes()
    {
        var payload = ZbrtFrameCodec.EncodeError(1, "x");
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.DecodeError(WithTrailing(payload)));
    }

    [Fact]
    public void PayloadReader_RejectsLengthBeyondBuffer()
    {
        // string length prefix claims 1 MiB but buffer is short
        var payload = new byte[] { 0x00, 0x10, 0x00, 0x00, 0x61 };
        Assert.Throws<DecodeException>(() => ZbrtFrameCodec.DecodeHello(payload));
    }
}
