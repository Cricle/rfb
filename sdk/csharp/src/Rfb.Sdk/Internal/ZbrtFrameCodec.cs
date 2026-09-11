using System.Buffers.Binary;
using System.Text;

namespace Rfb.Sdk.Internal;

/// <summary>ZBRT v1 frame kinds (PROTOCOL.md §3.2).</summary>
internal enum ZbrtKind : byte
{
    Hello = 1,
    HelloAck = 2,
    Execute = 3,
    Output = 4,
    Exit = 5,
    Cancel = 6,
    CancelAck = 7,
    Fs = 8,
    FsResult = 9,
    Health = 10,
    HealthAck = 11,
    Error = 12,
    Result = 13,
}

/// <summary>One ZBRT v1 frame: 28-byte header + payload.</summary>
internal sealed class ZbrtFrame
{
    public ZbrtKind Kind { get; init; }
    public ushort Flags { get; init; }
    public byte[] RequestId { get; init; } = new byte[16];
    public byte[] Payload { get; init; } = [];
}

/// <summary>Decoded ZBRT HelloAck payload.</summary>
internal sealed record ZbrtHelloAck(string Server, IReadOnlyList<string> Capabilities);

/// <summary>Decoded ZBRT Execute payload.</summary>
internal sealed record ZbrtExecute(IReadOnlyList<string> Argv, string? Cwd, byte[] Stdin, uint TimeoutMs);

/// <summary>Decoded ZBRT Cancel payload (legacy payloads carry no target byte → Target null).</summary>
internal sealed record ZbrtCancel(string? Reason, byte[]? Target);

/// <summary>
/// Growable single-buffer payload encoder (mirror of PayloadReader). Builds each
/// payload in one byte[] with no intermediate copies; strings encode directly
/// into the destination buffer.
/// </summary>
internal ref struct PayloadWriter
{
    private byte[] _buf;
    private int _len;

    internal PayloadWriter(int capacity) => (_buf, _len) = (new byte[capacity < 16 ? 16 : capacity], 0);

    internal readonly byte[] ToArray() => _buf.AsSpan(0, _len).ToArray();

    private void Ensure(int extra)
    {
        if (_len + extra > _buf.Length)
        {
            var grown = new byte[Math.Max(_buf.Length * 2, _len + extra)];
            _buf.AsSpan(0, _len).CopyTo(grown);
            _buf = grown;
        }
    }

    internal void U8(byte v) { Ensure(1); _buf[_len++] = v; }

    internal void Flag(bool v) => U8(v ? (byte)1 : (byte)0);

    internal void U32(uint v) { Ensure(4); BinaryPrimitives.WriteUInt32BigEndian(_buf.AsSpan(_len, 4), v); _len += 4; }

    internal void I32(int v) { Ensure(4); BinaryPrimitives.WriteInt32BigEndian(_buf.AsSpan(_len, 4), v); _len += 4; }

    internal void Raw(ReadOnlySpan<byte> raw) { Ensure(raw.Length); raw.CopyTo(_buf.AsSpan(_len)); _len += raw.Length; }

    internal void Bytes(ReadOnlySpan<byte> raw) { Ensure(4 + raw.Length); U32((uint)raw.Length); Raw(raw); }

    internal void String(string s)
    {
        var n = Encoding.UTF8.GetByteCount(s);
        Ensure(4 + n);
        U32((uint)n);
        Encoding.UTF8.GetBytes(s).CopyTo(_buf.AsSpan(_len));
        _len += n;
    }

    /// <summary>Flag byte + length-prefixed string, or a 0 flag when null.</summary>
    internal void OptString(string? s)
    {
        if (s is not null) { U8(1); String(s); } else { U8(0); }
    }
}

/// <summary>
/// Strict ZBRT v1 wire codec (PROTOCOL.md §3; mirror of rfb-runtime/src/zeroboot_protocol.rs).
/// Frame decode rejects bad magic/version, flags ≠ 0, unknown kinds, oversize and truncated
/// payloads. Payload codecs reject both truncation and trailing bytes.
/// </summary>
internal static class ZbrtFrameCodec
{
    public const int HeaderLen = 28;
    public const int MaxPayload = 16 * 1024 * 1024;
    public const byte Version = 1;
    public static readonly byte[] Magic = [(byte)'Z', (byte)'B', (byte)'R', (byte)'T'];

    // ---- frame ------------------------------------------------------------

    public static byte[] Encode(ZbrtFrame frame)
    {
        if (frame.Flags != 0)
        {
            throw new DecodeException("invalid frame: unsupported frame flags");
        }

        if (frame.Payload.Length > MaxPayload)
        {
            throw new DecodeException("invalid frame: payload too large");
        }

        if (frame.RequestId.Length != 16)
        {
            throw new DecodeException("invalid frame: request id must be 16 bytes");
        }

        var bytes = new byte[HeaderLen + frame.Payload.Length];
        Magic.CopyTo(bytes, 0);
        bytes[4] = Version;
        bytes[5] = (byte)frame.Kind;
        BinaryPrimitives.WriteUInt16BigEndian(bytes.AsSpan(6, 2), frame.Flags);
        frame.RequestId.CopyTo(bytes, 8);
        BinaryPrimitives.WriteUInt32BigEndian(bytes.AsSpan(24, 4), (uint)frame.Payload.Length);
        frame.Payload.CopyTo(bytes, HeaderLen);
        return bytes;
    }

    /// <summary>Decode one whole frame from a buffer. Buffer must contain exactly header+payload.</summary>
    public static ZbrtFrame Decode(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length < HeaderLen)
        {
            throw new DecodeException("truncated frame");
        }

        if (!bytes[..4].SequenceEqual(Magic) || bytes[4] != Version)
        {
            throw new DecodeException("invalid magic or version");
        }

        var kindByte = bytes[5];
        if (!Enum.IsDefined(typeof(ZbrtKind), kindByte))
        {
            throw new DecodeException("unknown frame kind");
        }

        var flags = BinaryPrimitives.ReadUInt16BigEndian(bytes[6..8]);
        if (flags != 0)
        {
            throw new DecodeException("unsupported frame flags");
        }

        var payloadLen = BinaryPrimitives.ReadUInt32BigEndian(bytes[24..28]);
        if (payloadLen > MaxPayload)
        {
            throw new DecodeException("payload too large");
        }

        if (bytes.Length < HeaderLen + payloadLen)
        {
            throw new DecodeException("truncated frame");
        }

        return new ZbrtFrame
        {
            Kind = (ZbrtKind)kindByte,
            Flags = flags,
            RequestId = bytes.Slice(8, 16).ToArray(),
            Payload = bytes.Slice(HeaderLen, (int)payloadLen).ToArray(),
        };
    }

    // ---- payload primitives ----------------------------------------------
    // (kept for golden-vector tests; SDK encode paths use PayloadWriter)

    public static void PutString(List<byte> outBytes, string s)
        => PutBytes(outBytes, Encoding.UTF8.GetBytes(s));

    public static void PutBytes(List<byte> outBytes, ReadOnlySpan<byte> raw)
    {
        Span<byte> len = stackalloc byte[4];
        BinaryPrimitives.WriteUInt32BigEndian(len, (uint)raw.Length);
        foreach (var b in len) outBytes.Add(b);
        foreach (var b in raw) outBytes.Add(b);
    }

    private ref struct PayloadReader
    {
        private ReadOnlySpan<byte> _b;

        internal PayloadReader(ReadOnlySpan<byte> payload) => _b = payload;

        internal readonly ReadOnlySpan<byte> Remaining => _b;

        internal byte U8()
        {
            if (_b.Length < 1)
            {
                throw new DecodeException("truncated payload");
            }

            var v = _b[0];
            _b = _b[1..];
            return v;
        }

        internal bool Flag() => U8() != 0;

        internal ReadOnlySpan<byte> Take(int n)
        {
            if (_b.Length < n)
            {
                throw new DecodeException("truncated payload");
            }

            var v = _b[..n];
            _b = _b[n..];
            return v;
        }

        internal uint U32() => BinaryPrimitives.ReadUInt32BigEndian(Take(4));

        internal int I32() => BinaryPrimitives.ReadInt32BigEndian(Take(4));

        internal byte[] Bytes() => Take((int)U32()).ToArray();

        internal string String()
        {
            var raw = Bytes();
            try
            {
                return Encoding.UTF8.GetString(raw);
            }
            catch (DecoderFallbackException)
            {
                throw new DecodeException("invalid utf8");
            }
        }

        internal void EnsureEmpty()
        {
            if (!_b.IsEmpty)
            {
                throw new DecodeException("trailing payload");
            }
        }
    }

    // ---- Hello / HelloAck -------------------------------------------------

    public static byte[] EncodeHello(string client, IReadOnlyList<string> capabilities)
    {
        var w = new PayloadWriter(64);
        w.String(client);
        AppendCaps(ref w, capabilities);
        return w.ToArray();
    }

    public static ZbrtFrame HelloFrame(byte[] requestId, string client, IReadOnlyList<string> capabilities) =>
        new() { Kind = ZbrtKind.Hello, RequestId = requestId, Payload = EncodeHello(client, capabilities) };

    public static (string Client, List<string> Capabilities) DecodeHello(ReadOnlySpan<byte> payload)
    {
        var r = new PayloadReader(payload);
        var client = r.String();
        var caps = ReadCaps(ref r);
        r.EnsureEmpty();
        return (client, caps);
    }

    public static byte[] EncodeHelloAck(string server, IReadOnlyList<string> capabilities)
    {
        var w = new PayloadWriter(64);
        w.String(server);
        AppendCaps(ref w, capabilities);
        return w.ToArray();
    }

    public static ZbrtHelloAck DecodeHelloAck(ReadOnlySpan<byte> payload)
    {
        var r = new PayloadReader(payload);
        var server = r.String();
        var caps = ReadCaps(ref r);
        r.EnsureEmpty();
        return new ZbrtHelloAck(server, caps);
    }

    private static void AppendCaps(ref PayloadWriter w, IReadOnlyList<string> capabilities)
    {
        if (capabilities.Count > byte.MaxValue)
        {
            throw new DecodeException("too many capabilities");
        }

        w.U8((byte)capabilities.Count);
        foreach (var cap in capabilities) w.String(cap);
    }

    private static List<string> ReadCaps(ref PayloadReader r)
    {
        var n = r.U8();
        var caps = new List<string>(n);
        for (var i = 0; i < n; i++)
        {
            caps.Add(r.String());
        }

        return caps;
    }

    // ---- Execute ----------------------------------------------------------

    public static byte[] EncodeExecute(IReadOnlyList<string> argv, string? cwd, byte[] stdin, uint timeoutMs)
    {
        if (argv.Count > byte.MaxValue)
        {
            throw new DecodeException("too many arguments");
        }

        var w = new PayloadWriter(64 + stdin.Length);
        w.U8((byte)argv.Count);
        foreach (var arg in argv) w.String(arg);
        w.OptString(cwd);
        w.Bytes(stdin);
        w.U32(timeoutMs);
        return w.ToArray();
    }

    public static ZbrtExecute DecodeExecute(ReadOnlySpan<byte> payload)
    {
        var r = new PayloadReader(payload);
        var argc = r.U8();
        var argv = new List<string>(argc);
        for (var i = 0; i < argc; i++)
        {
            argv.Add(r.String());
        }

        var cwd = r.Flag() ? r.String() : null;
        var stdin = r.Bytes();
        var timeoutMs = r.U32();
        r.EnsureEmpty();
        return new ZbrtExecute(argv, cwd, stdin, timeoutMs);
    }

    // ---- Output / Exit ----------------------------------------------------

    public static byte[] EncodeOutput(byte stream, byte[] data)
    {
        var w = new PayloadWriter(8 + data.Length);
        w.U8(stream);
        w.Bytes(data);
        return w.ToArray();
    }

    public static (byte Stream, byte[] Data) DecodeOutput(ReadOnlySpan<byte> payload)
    {
        var r = new PayloadReader(payload);
        var stream = r.U8();
        var data = r.Bytes();
        r.EnsureEmpty();
        return (stream, data);
    }

    public static byte[] EncodeExit(int code, uint? signal)
    {
        var w = new PayloadWriter(16);
        w.I32(code);
        if (signal.HasValue) { w.U8(1); w.U32(signal.Value); } else { w.U8(0); }
        return w.ToArray();
    }

    public static (int Code, uint? Signal) DecodeExit(ReadOnlySpan<byte> payload)
    {
        var r = new PayloadReader(payload);
        var code = r.I32();
        uint? signal = r.Flag() ? r.U32() : null;
        r.EnsureEmpty();
        return (code, signal);
    }

    // ---- Cancel -----------------------------------------------------------

    /// <summary>Modern encoding always carries the target flag byte; target null → flag 0.</summary>
    public static byte[] EncodeCancel(string? reason, byte[]? target)
    {
        var w = new PayloadWriter(64);
        w.OptString(reason);
        if (target is not null)
        {
            if (target.Length != 16)
            {
                throw new DecodeException("cancel target must be 16 bytes");
            }

            w.U8(1);
            w.Raw(target);
        }
        else
        {
            w.U8(0);
        }

        return w.ToArray();
    }

    /// <summary>Legacy payloads stop right after the reason flag → target null.</summary>
    public static ZbrtCancel DecodeCancel(ReadOnlySpan<byte> payload)
    {
        var r = new PayloadReader(payload);
        var reason = r.Flag() ? r.String() : null;
        byte[]? target;
        if (r.Remaining.IsEmpty)
        {
            target = null; // legacy: no target byte at all
        }
        else if (r.Flag())
        {
            target = r.Take(16).ToArray();
        }
        else
        {
            target = null;
        }

        r.EnsureEmpty();
        return new ZbrtCancel(reason, target);
    }

    // ---- Fs ---------------------------------------------------------------

    public static byte[] EncodeFs(byte op, string path, byte[] jsonData)
    {
        var w = new PayloadWriter(64 + jsonData.Length);
        w.U8(op);
        w.String(path);
        w.Bytes(jsonData);
        return w.ToArray();
    }

    public static (byte Op, string Path, byte[] JsonData) DecodeFs(ReadOnlySpan<byte> payload)
    {
        var r = new PayloadReader(payload);
        var op = r.U8();
        var path = r.String();
        var data = r.Bytes();
        r.EnsureEmpty();
        return (op, path, data);
    }

    // ---- Health -----------------------------------------------------------

    public static byte[] EncodeHealth(bool healthy, string? message)
    {
        var w = new PayloadWriter(16);
        w.Flag(healthy);
        w.OptString(message);
        return w.ToArray();
    }

    public static (bool Healthy, string? Message) DecodeHealth(ReadOnlySpan<byte> payload)
    {
        var r = new PayloadReader(payload);
        var healthy = r.Flag();
        var message = r.Flag() ? r.String() : null;
        r.EnsureEmpty();
        return (healthy, message);
    }

    // ---- Error ------------------------------------------------------------

    public static byte[] EncodeError(uint code, string message)
    {
        var w = new PayloadWriter(64);
        w.U32(code);
        w.String(message);
        return w.ToArray();
    }

    public static (uint Code, string Message) DecodeError(ReadOnlySpan<byte> payload)
    {
        var r = new PayloadReader(payload);
        var code = r.U32();
        var message = r.String();
        r.EnsureEmpty();
        return (code, message);
    }
}
