using Rfb.Sdk.Internal;
using Xunit;

namespace Rfb.Sdk.Tests;

/// <summary>PROTOCOL.md §4 golden vectors — all 8, encode AND decode.</summary>
public class GoldenVectorTests
{
    private static readonly byte[] RequestId = Hex.ToBytes("000102030405060708090a0b0c0d0e0f");

    private const string HelloHex =
        "5a42525401010000000102030405060708090a0b0c0d0e0f000000220000000873646b2d746573740200000007657865637574650000000673747265616d";

    private const string HelloAckHex =
        "5a42525401020000000102030405060708090a0b0c0d0e0f0000005a000000127266622d7a65726f626f6f742d67756573740600000007657865637574650000000673747265616d00000008646561646c696e65000000066865616c74680000000663616e63656c0000000a66696c6573797374656d";

    private const string ExecuteHex =
        "5a42525401030000000102030405060708090a0b0c0d0e0f0000002902000000046563686f000000026869010000000a2f776f726b737061636500000003616263000005dc";

    private const string OutputHex =
        "5a42525401040000000102030405060708090a0b0c0d0e0f0000000e0100000009657272206c696e650a";

    private const string ExitHex =
        "5a42525401050000000102030405060708090a0b0c0d0e0f000000050000000000";

    private const string CancelHex =
        "5a42525401060000000102030405060708090a0b0c0d0e0f0000000a01000000047573657200";

    private const string CancelLegacyHex =
        "5a42525401060000000102030405060708090a0b0c0d0e0f00000009010000000475736572";

    private const string ErrorHex =
        "5a425254010c0000000102030405060708090a0b0c0d0e0f00000015000000010000000d6172677620697320656d707479";

    // ---- encode direction --------------------------------------------------

    [Fact]
    public void Encode_Hello_MatchesGoldenVector()
    {
        var frame = ZbrtFrameCodec.HelloFrame(RequestId, "sdk-test", new[] { "execute", "stream" });
        Assert.Equal(HelloHex, Hex.Of(ZbrtFrameCodec.Encode(frame)));
    }

    [Fact]
    public void Encode_HelloAck_MatchesGoldenVector()
    {
        var frame = new ZbrtFrame
        {
            Kind = ZbrtKind.HelloAck,
            RequestId = RequestId,
            Payload = ZbrtFrameCodec.EncodeHelloAck("rfb-zeroboot-guest", new[] {
                "execute", "stream", "deadline", "health", "cancel", "filesystem",
            }),
        };
        Assert.Equal(HelloAckHex, Hex.Of(ZbrtFrameCodec.Encode(frame)));
    }

    [Fact]
    public void Encode_Execute_MatchesGoldenVector()
    {
        var frame = new ZbrtFrame
        {
            Kind = ZbrtKind.Execute,
            RequestId = RequestId,
            Payload = ZbrtFrameCodec.EncodeExecute(
                new[] { "echo", "hi" }, "/workspace", "abc"u8.ToArray(), 1500),
        };
        Assert.Equal(ExecuteHex, Hex.Of(ZbrtFrameCodec.Encode(frame)));
    }

    [Fact]
    public void Encode_Output_MatchesGoldenVector()
    {
        var frame = new ZbrtFrame
        {
            Kind = ZbrtKind.Output,
            RequestId = RequestId,
            Payload = ZbrtFrameCodec.EncodeOutput(1, "err line\n"u8.ToArray()),
        };
        Assert.Equal(OutputHex, Hex.Of(ZbrtFrameCodec.Encode(frame)));
    }

    [Fact]
    public void Encode_Exit_MatchesGoldenVector()
    {
        var frame = new ZbrtFrame
        {
            Kind = ZbrtKind.Exit,
            RequestId = RequestId,
            Payload = ZbrtFrameCodec.EncodeExit(0, null),
        };
        Assert.Equal(ExitHex, Hex.Of(ZbrtFrameCodec.Encode(frame)));
    }

    [Fact]
    public void Encode_Cancel_MatchesGoldenVector()
    {
        var frame = new ZbrtFrame
        {
            Kind = ZbrtKind.Cancel,
            RequestId = RequestId,
            Payload = ZbrtFrameCodec.EncodeCancel("user", null),
        };
        Assert.Equal(CancelHex, Hex.Of(ZbrtFrameCodec.Encode(frame)));
    }

    [Fact]
    public void Encode_CancelLegacy_MatchesGoldenVector()
    {
        // Legacy form: payload ends right after the reason (no target flag byte).
        var payload = new List<byte> { 1 };
        ZbrtFrameCodec.PutString(payload, "user");
        var frame = new ZbrtFrame
        {
            Kind = ZbrtKind.Cancel,
            RequestId = RequestId,
            Payload = payload.ToArray(),
        };
        Assert.Equal(CancelLegacyHex, Hex.Of(ZbrtFrameCodec.Encode(frame)));
    }

    [Fact]
    public void Encode_Error_MatchesGoldenVector()
    {
        var frame = new ZbrtFrame
        {
            Kind = ZbrtKind.Error,
            RequestId = RequestId,
            Payload = ZbrtFrameCodec.EncodeError(1, "argv is empty"),
        };
        Assert.Equal(ErrorHex, Hex.Of(ZbrtFrameCodec.Encode(frame)));
    }

    // ---- decode direction --------------------------------------------------

    [Fact]
    public void Decode_Hello_MatchesGoldenVector()
    {
        var frame = ZbrtFrameCodec.Decode(Hex.ToBytes(HelloHex));
        Assert.Equal(ZbrtKind.Hello, frame.Kind);
        Assert.Equal(0, frame.Flags);
        Assert.Equal(RequestId, frame.RequestId);
        var (client, caps) = ZbrtFrameCodec.DecodeHello(frame.Payload);
        Assert.Equal("sdk-test", client);
        Assert.Equal(new[] { "execute", "stream" }, caps);
    }

    [Fact]
    public void Decode_HelloAck_MatchesGoldenVector()
    {
        var frame = ZbrtFrameCodec.Decode(Hex.ToBytes(HelloAckHex));
        Assert.Equal(ZbrtKind.HelloAck, frame.Kind);
        var ack = ZbrtFrameCodec.DecodeHelloAck(frame.Payload);
        Assert.Equal("rfb-zeroboot-guest", ack.Server);
        Assert.Equal(6, ack.Capabilities.Count);
        Assert.Equal(new[] { "execute", "stream", "deadline", "health", "cancel", "filesystem" }, ack.Capabilities);
    }

    [Fact]
    public void Decode_Execute_MatchesGoldenVector()
    {
        var frame = ZbrtFrameCodec.Decode(Hex.ToBytes(ExecuteHex));
        Assert.Equal(ZbrtKind.Execute, frame.Kind);
        var exec = ZbrtFrameCodec.DecodeExecute(frame.Payload);
        Assert.Equal(new[] { "echo", "hi" }, exec.Argv);
        Assert.Equal("/workspace", exec.Cwd);
        Assert.Equal("abc"u8.ToArray(), exec.Stdin);
        Assert.Equal(1500u, exec.TimeoutMs);
    }

    [Fact]
    public void Decode_Output_MatchesGoldenVector()
    {
        var frame = ZbrtFrameCodec.Decode(Hex.ToBytes(OutputHex));
        Assert.Equal(ZbrtKind.Output, frame.Kind);
        var (stream, data) = ZbrtFrameCodec.DecodeOutput(frame.Payload);
        Assert.Equal(1, stream);
        Assert.Equal("err line\n"u8.ToArray(), data);
    }

    [Fact]
    public void Decode_Exit_MatchesGoldenVector()
    {
        var frame = ZbrtFrameCodec.Decode(Hex.ToBytes(ExitHex));
        Assert.Equal(ZbrtKind.Exit, frame.Kind);
        var (code, signal) = ZbrtFrameCodec.DecodeExit(frame.Payload);
        Assert.Equal(0, code);
        Assert.Null(signal);
    }

    [Fact]
    public void Decode_Cancel_MatchesGoldenVector()
    {
        var frame = ZbrtFrameCodec.Decode(Hex.ToBytes(CancelHex));
        Assert.Equal(ZbrtKind.Cancel, frame.Kind);
        var cancel = ZbrtFrameCodec.DecodeCancel(frame.Payload);
        Assert.Equal("user", cancel.Reason);
        Assert.Null(cancel.Target);
    }

    [Fact]
    public void Decode_CancelLegacy_MatchesGoldenVector()
    {
        var frame = ZbrtFrameCodec.Decode(Hex.ToBytes(CancelLegacyHex));
        Assert.Equal(ZbrtKind.Cancel, frame.Kind);
        var cancel = ZbrtFrameCodec.DecodeCancel(frame.Payload);
        Assert.Equal("user", cancel.Reason);
        Assert.Null(cancel.Target); // legacy = no target byte
    }

    [Fact]
    public void Decode_Error_MatchesGoldenVector()
    {
        var frame = ZbrtFrameCodec.Decode(Hex.ToBytes(ErrorHex));
        Assert.Equal(ZbrtKind.Error, frame.Kind);
        var (code, message) = ZbrtFrameCodec.DecodeError(frame.Payload);
        Assert.Equal(1u, code);
        Assert.Equal("argv is empty", message);
    }
}
