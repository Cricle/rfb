using System.Buffers.Binary;
using System.Net.Sockets;
using System.Text.Json;

namespace Rfb.Sdk.Internal;

/// <summary>Result of one ZBRT Execute turn.</summary>
internal sealed record ZbrtExecOutcome(int ExitCode, byte[] Stdout, byte[] Stderr);

/// <summary>
/// ZBRT v1 TCP client (PROTOCOL.md §3.4; mirror of the host-side ZeroBoot session).
/// One active request per connection; a fresh 128-bit request id per request;
/// Output frames strictly precede the single terminal Exit/Error frame.
/// Internal only — never part of the public API.
/// </summary>
internal sealed class ZbrtTcpClient : IDisposable
{
    private readonly string _host;
    private readonly int _port;
    private readonly TimeSpan _timeout;
    private readonly byte[] _header = new byte[ZbrtFrameCodec.HeaderLen];
    private TcpClient? _tcp;
    private NetworkStream? _stream;

    public ZbrtTcpClient(string address, TimeSpan timeout)
    {
        (_host, _port) = WireJson.ParseGuestAddress(address);
        _timeout = timeout;
    }

    public async Task ConnectAsync()
    {
        if (_tcp is not null)
        {
            return;
        }

        var tcp = new TcpClient();
        using var cts = new CancellationTokenSource(_timeout);
        try
        {
            await TcpCompat.ConnectAsync(tcp, _host, _port, cts.Token).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            tcp.Dispose();
            throw new TransportException("guest connect timeout");
        }
        catch (SocketException e)
        {
            tcp.Dispose();
            throw new TransportException($"guest connect failed: {e.Message}", e);
        }

        _tcp = tcp;
        tcp.NoDelay = true;
        _stream = tcp.GetStream();
    }

    /// <summary>Optional handshake: Hello → HelloAck.</summary>
    public async Task<ZbrtHelloAck> HelloAsync(string client, IReadOnlyList<string> capabilities)
    {
        await ConnectAsync().ConfigureAwait(false);
        var reply = await RoundTripAsync(ZbrtFrameCodec.HelloFrame(NewRequestId(), client, capabilities)).ConfigureAwait(false);
        if (reply.Kind != ZbrtKind.HelloAck)
        {
            throw UnexpectedKind(reply, ZbrtKind.HelloAck);
        }

        return ZbrtFrameCodec.DecodeHelloAck(reply.Payload);
    }

    /// <summary>Execute: 0..n Output frames then exactly one terminal Exit (or Error frame).</summary>
    public async Task<ZbrtExecOutcome> ExecuteAsync(IReadOnlyList<string> argv, string? cwd, byte[] stdin, uint timeoutMs)
    {
        await ConnectAsync().ConfigureAwait(false);
        var payload = ZbrtFrameCodec.EncodeExecute(argv, cwd, stdin, timeoutMs);
        var request = new ZbrtFrame { Kind = ZbrtKind.Execute, RequestId = NewRequestId(), Payload = payload };
        try
        {
            await WriteFrameAsync(request).ConfigureAwait(false);

            var stdout = new List<byte>();
            var stderr = new List<byte>();
            while (true)
            {
                var frame = await ReadFrameAsync().ConfigureAwait(false);
                RequireRequestId(frame, request.RequestId);
                switch (frame.Kind)
                {
                    case ZbrtKind.Output:
                        {
                            var (stream, data) = ZbrtFrameCodec.DecodeOutput(frame.Payload);
                            if (stream == 0)
                            {
                                stdout.AddRange(data);
                            }
                            else if (stream == 1)
                            {
                                stderr.AddRange(data);
                            }
                            else
                            {
                                throw new DecodeException("invalid output stream");
                            }

                            break;
                        }
                    case ZbrtKind.Exit:
                        {
                            var (code, _) = ZbrtFrameCodec.DecodeExit(frame.Payload);
                            return new ZbrtExecOutcome(code, stdout.ToArray(), stderr.ToArray());
                        }
                    case ZbrtKind.Error:
                        throw RemoteError(frame);
                    default:
                        throw UnexpectedKind(frame, "Output/Exit/Error");
                }
            }
        }
        catch (TransportException)
        {
            // A timed-out read leaves the connection mid-turn: drop it so the
            // next request cannot consume stale frames.
            ResetConnection();
            throw;
        }
    }

    /// <summary>Health → HealthAck; returns the guest's healthy flag.</summary>
    public async Task<bool> HealthAsync()
    {
        await ConnectAsync().ConfigureAwait(false);
        var payload = ZbrtFrameCodec.EncodeHealth(healthy: true, message: null);
        var request = new ZbrtFrame { Kind = ZbrtKind.Health, RequestId = NewRequestId(), Payload = payload };
        var reply = await RoundTripAsync(request).ConfigureAwait(false);
        if (reply.Kind != ZbrtKind.HealthAck)
        {
            throw UnexpectedKind(reply, ZbrtKind.HealthAck);
        }

        var (healthy, _) = ZbrtFrameCodec.DecodeHealth(reply.Payload);
        return healthy;
    }

    /// <summary>Route one filesystem RPC: Fs frame → FsResult JSON (or Error frame).</summary>
    public async Task<JsonElement> FsAsync(byte op, string path, byte[] jsonArgs)
    {
        await ConnectAsync().ConfigureAwait(false);
        var payload = ZbrtFrameCodec.EncodeFs(op, path, jsonArgs);
        var request = new ZbrtFrame { Kind = ZbrtKind.Fs, RequestId = NewRequestId(), Payload = payload };
        var reply = await RoundTripAsync(request).ConfigureAwait(false);
        if (reply.Kind == ZbrtKind.Error)
        {
            throw RemoteError(reply);
        }

        if (reply.Kind != ZbrtKind.FsResult)
        {
            throw UnexpectedKind(reply, ZbrtKind.FsResult);
        }

        if (reply.Payload.Length == 0)
        {
            throw new DecodeException("empty fs result");
        }

        try
        {
            return JsonDocument.Parse(reply.Payload).RootElement.Clone();
        }
        catch (JsonException e)
        {
            throw new DecodeException($"invalid fs result JSON: {e.Message}");
        }
    }

    /// <summary>Idempotent Cancel; returns the CancelAck reply frame.</summary>
    public async Task<ZbrtFrame> CancelAsync(byte[]? target, string? reason)
    {
        await ConnectAsync().ConfigureAwait(false);
        var payload = ZbrtFrameCodec.EncodeCancel(reason, target);
        var request = new ZbrtFrame { Kind = ZbrtKind.Cancel, RequestId = NewRequestId(), Payload = payload };
        var reply = await RoundTripAsync(request).ConfigureAwait(false);
        if (reply.Kind == ZbrtKind.Error)
        {
            throw RemoteError(reply);
        }

        if (reply.Kind != ZbrtKind.CancelAck)
        {
            throw UnexpectedKind(reply, ZbrtKind.CancelAck);
        }

        return reply;
    }

    /// <summary>Start a stream session for one Execute request.</summary>
    public async Task<ZbrtStreamSession> StreamAsync(IReadOnlyList<string> argv, string? cwd)
    {
        await ConnectAsync().ConfigureAwait(false);
        var payload = ZbrtFrameCodec.EncodeExecute(argv, cwd, [], timeoutMs: 0);
        var request = new ZbrtFrame { Kind = ZbrtKind.Execute, RequestId = NewRequestId(), Payload = payload };
        await WriteFrameAsync(request).ConfigureAwait(false);
        return new ZbrtStreamSession(this, request.RequestId);
    }

    private async Task<ZbrtFrame> RoundTripAsync(ZbrtFrame request)
    {
        try
        {
            await WriteFrameAsync(request).ConfigureAwait(false);
            var reply = await ReadFrameAsync().ConfigureAwait(false);
            RequireRequestId(reply, request.RequestId);
            return reply;
        }
        catch (TransportException)
        {
            ResetConnection();
            throw;
        }
    }

    /// <summary>
    /// Drop the TCP connection so the next operation reconnects cleanly. A
    /// timed-out read leaves unread frames buffered on the socket; reusing it
    /// would poison the next request with stale frames (id mismatch).
    /// </summary>
    internal void ResetConnection()
    {
        try
        {
            _stream?.Dispose();
        }
        catch (IOException)
        {
            // Disposing a broken stream may throw; the connection is going away anyway.
        }

        _stream = null;
        _tcp?.Dispose();
        _tcp = null;
    }

    internal async Task WriteFrameAsync(ZbrtFrame frame)
    {
        var bytes = ZbrtFrameCodec.Encode(frame);
        using var cts = new CancellationTokenSource(_timeout);
        try
        {
            // one write per frame (NetworkStream is unbuffered; Flush is a no-op)
            await _stream!.WriteAsync(bytes, 0, bytes.Length, cts.Token).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            throw new TransportException("guest write timeout");
        }
        catch (SocketException e)
        {
            throw new TransportException($"guest write failed: {e.Message}");
        }
    }

    internal async Task<ZbrtFrame> ReadFrameAsync()
    {
        using var cts = new CancellationTokenSource(_timeout);
        await ReadExactlyAsync(_header, cts.Token).ConfigureAwait(false);
        var payloadLen = BinaryPrimitives.ReadUInt32BigEndian(_header.AsSpan(24, 4));
        if (payloadLen > ZbrtFrameCodec.MaxPayload)
        {
            throw new DecodeException("payload too large");
        }

        // Read header+payload into ONE buffer: copy the header, then read the
        // payload straight after it (no intermediate payload array, no join copy).
        var full = new byte[ZbrtFrameCodec.HeaderLen + payloadLen];
        _header.CopyTo(full, 0);
        await ReadExactlyAsync(full.AsMemory(ZbrtFrameCodec.HeaderLen), cts.Token).ConfigureAwait(false);
        return ZbrtFrameCodec.Decode(full);
    }

    private async Task ReadExactlyAsync(Memory<byte> buffer, CancellationToken token)
    {
        var offset = 0;
        while (offset < buffer.Length)
        {
            int n;
            try
            {
                n = await _stream!.ReadAsync(buffer.Slice(offset), token).ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                throw new TransportException("guest response timeout");
            }
            catch (SocketException e)
            {
                throw new TransportException($"guest read failed: {e.Message}");
            }

            if (n == 0)
            {
                throw new TransportException("guest closed connection");
            }

            offset += n;
        }
    }

    private static void RequireRequestId(ZbrtFrame frame, byte[] requestId)
    {
        if (!frame.RequestId.AsSpan().SequenceEqual(requestId))
        {
            throw new DecodeException("frame request id mismatch");
        }
    }

    private static RemoteException RemoteError(ZbrtFrame frame)
    {
        var (_, message) = ZbrtFrameCodec.DecodeError(frame.Payload);
        return new RemoteException(message);
    }

    private static DecodeException UnexpectedKind(ZbrtFrame frame, object expected) =>
        new($"unexpected frame kind {frame.Kind} (expected {expected})");

    /// <summary>Fresh 128-bit request id per request.</summary>
    internal static byte[] NewRequestId() =>
        RandomBytes(16);

    private static byte[] RandomBytes(int count)
    {
        // Instance API exists on every TFM (the static GetBytes(int) is net6+).
        using var rng = System.Security.Cryptography.RandomNumberGenerator.Create();
        var buffer = new byte[count];
        rng.GetBytes(buffer);
        return buffer;
    }

    public void Dispose()
    {
        _stream?.Dispose();
        _tcp?.Dispose();
    }
}

/// <summary>One ZBRT stream session: Output frames → events, terminal Exit, targeted Cancel on stop.</summary>
internal sealed class ZbrtStreamSession : IDisposable
{
    private readonly ZbrtTcpClient _client;
    private readonly byte[] _requestId;
    private bool _terminal;
    private bool _stopped;

    internal ZbrtStreamSession(ZbrtTcpClient client, byte[] requestId)
    {
        _client = client;
        _requestId = requestId;
    }

    /// <summary>Next stream event; Exit ends the session (subsequent calls return null).</summary>
    public async Task<StreamEvent?> NextEventAsync()
    {
        if (_terminal)
        {
            return null;
        }

        while (true)
        {
            ZbrtFrame frame;
            try
            {
                frame = await _client.ReadFrameAsync().ConfigureAwait(false);
            }
            catch (TransportException e) when (e.Message == "guest closed connection")
            {
                return null; // clean close
            }

            if (!frame.RequestId.AsSpan().SequenceEqual(_requestId))
            {
                // PROTOCOL.md §3.4: a reply must echo the request id. The stop
                // path already sends Cancel with this turn's id, so a
                // CancelAck also matches; anything else is a desync.
                throw new DecodeException("frame request id mismatch");
            }

            switch (frame.Kind)
            {
                case ZbrtKind.Output:
                    {
                        var (stream, data) = ZbrtFrameCodec.DecodeOutput(frame.Payload);
                        if (stream == 0)
                        {
                            return new StreamEvent(StreamEventKind.Stdout, data, null);
                        }

                        if (stream == 1)
                        {
                            return new StreamEvent(StreamEventKind.Stderr, data, null);
                        }

                        throw new DecodeException("invalid output stream");
                    }
                case ZbrtKind.Exit:
                    {
                        var (code, _) = ZbrtFrameCodec.DecodeExit(frame.Payload);
                        _terminal = true;
                        return new StreamEvent(StreamEventKind.Exit, [], code);
                    }
                case ZbrtKind.Error:
                    {
                        var (_, message) = ZbrtFrameCodec.DecodeError(frame.Payload);
                        throw new RemoteException(message);
                    }
                case ZbrtKind.CancelAck:
                    continue;
                default:
                    continue;
            }
        }
    }

    /// <summary>ZBRT v1 has no stdin channel; sending input is a remote error.</summary>
    public Task SendInputAsync(string input)
    {
        if (_terminal || _stopped)
        {
            throw new RemoteException("guest stream is no longer running");
        }

        throw new RemoteException("guest stream does not support stdin over zbrt transport");
    }

    /// <summary>Idempotent stop: send Cancel (reason "stop") targeting this request
    /// and await the CancelAck. Straggler Output/Exit frames for the cancelled
    /// turn are skipped while waiting, mirroring the Python/Java baselines; the
    /// caller's next NextEventAsync still delivers the terminal Exit.</summary>
    public async Task StopAsync()
    {
        if (_terminal || _stopped)
        {
            return;
        }

        _stopped = true;
        var payload = ZbrtFrameCodec.EncodeCancel("stop", _requestId);
        var cancel = new ZbrtFrame { Kind = ZbrtKind.Cancel, RequestId = _requestId, Payload = payload };
        await _client.WriteFrameAsync(cancel).ConfigureAwait(false);
        while (true)
        {
            ZbrtFrame frame;
            try
            {
                frame = await _client.ReadFrameAsync().ConfigureAwait(false);
            }
            catch (TransportException e) when (e.Message == "guest closed connection")
            {
                // Clean close: the stream is over; the ack will never arrive.
                return;
            }

            if (!frame.RequestId.AsSpan().SequenceEqual(_requestId))
            {
                throw new DecodeException("frame request id mismatch");
            }

            if (frame.Kind == ZbrtKind.CancelAck)
            {
                return;
            }

            if (frame.Kind == ZbrtKind.Error)
            {
                var (_, message) = ZbrtFrameCodec.DecodeError(frame.Payload);
                throw new RemoteException(message);
            }

            if (frame.Kind == ZbrtKind.Output || frame.Kind == ZbrtKind.Exit)
            {
                continue;
            }

            throw new DecodeException($"expected CancelAck, got frame kind {(int)frame.Kind}");
        }
    }

    /// <summary>
    /// Release the underlying connection (idempotent). A stream occupies the
    /// client's single turn, so after disposal the socket state is unknown;
    /// closing it makes the next operation reconnect cleanly.
    /// </summary>
    public void Dispose() => _client.ResetConnection();
}
