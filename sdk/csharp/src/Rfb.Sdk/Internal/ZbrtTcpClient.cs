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
    /// <summary>Client name sent in the mandatory connection Hello.</summary>
    internal const string HelloClientName = "rfb-sdk-csharp";

    /// <summary>ZBRT v1 capabilities declared by this SDK (PROTOCOL.md §3.4).</summary>
    internal static readonly string[] V1Capabilities =
        { "execute", "stream", "deadline", "health", "cancel", "filesystem" };

    /// <summary>Aggregate cap on one exec turn's captured output (mirrors the
    /// Rust baseline MAX_GUEST_PAYLOAD_BYTES: a chatty guest must not grow
    /// host memory without bound).</summary>
    private const long MaxTurnOutputBytes = 16L * 1024 * 1024;

    /// <summary>TCP 形态的地址（uds 形态下为空 —— 该路径不触 TCP）。</summary>
    private readonly string _host = "";
    private readonly int _port;
    private readonly TimeSpan _timeout;
    private readonly byte[] _header = new byte[ZbrtFrameCodec.HeaderLen];
    private TcpClient? _tcp;
    private readonly string? _udsSocketPath;
    /// <summary>UDS 形态（uds:路径[@端口]）的宿主侧 socket。</summary>
    private Socket? _udsSocket;
    /// <summary>UDS 直拨时的 guest vsock 端口（CONNECT 前导用）。</summary>
    private int _udsGuestPort;
    private NetworkStream? _stream;
    private readonly string _address;


    public ZbrtTcpClient(string address, TimeSpan timeout)
    {
        if (address.StartsWith("uds:", StringComparison.Ordinal))
        {
            // 直拨 FC 的 vsock relay UDS（无 TCP/中继跳）。
            var rest = address[4..];
            var at = rest.LastIndexOf('@');
            _udsSocketPath = at >= 0 ? rest[..at] : rest;
            _udsGuestPort = at >= 0
                ? int.Parse(rest[(at + 1)..], System.Globalization.CultureInfo.InvariantCulture)
                : 5000;
        }
        else
        {
            (_host, _port) = WireJson.ParseGuestAddress(address);
        }
        _address = address;
        _timeout = timeout;
    }

    public async Task ConnectAsync()
    {
        if (_tcp is not null || _stream is not null)
        {
            return;
        }

        if (_udsSocketPath is not null)
        {
#if NET
            await ConnectUdsAsync(_udsSocketPath, _udsGuestPort).ConfigureAwait(false);
            await HandshakeAsync().ConfigureAwait(false);
            return;
#else
            throw new TransportException(
                "uds: guest addresses need the net8.0 target (UnixDomainSocketEndPoint)");
#endif
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

        // Cross-language contract: every ZBRT connection opens with a mandatory
        // Hello → HelloAck handshake before any request. A peer that cannot
        // complete it is a transport failure.
        await HandshakeAsync().ConfigureAwait(false);
    }

#if NET
    /// <summary>UDS 直拨 + FC 的 CONNECT 前导（net8.0+：UnixDomainSocketEndPoint）。</summary>
    private async Task ConnectUdsAsync(string path, int guestPort)
    {
        using var cts = new CancellationTokenSource(_timeout);
        var socket = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
        try
        {
            await socket.ConnectAsync(new UnixDomainSocketEndPoint(path), cts.Token)
                .ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            socket.Dispose();
            throw new TransportException("guest connect timeout");
        }
        catch (SocketException e)
        {
            socket.Dispose();
            throw new TransportException($"guest connect failed: {e.Message}", e);
        }

        // AF_UNIX 不支持 TCP_NODELAY（uds 本无 Nagle）；SO_RCVTIMEO/SO_SNDTIMEO 可用。
        socket.ReceiveTimeout = (int)_timeout.TotalMilliseconds;
        socket.SendTimeout = (int)_timeout.TotalMilliseconds;
        _stream = new NetworkStream(socket, ownsSocket: true);
        _udsSocket = socket;
        // FC 的 vsock relay UDS 不是透明字节流：先 CONNECT 前导。
        var command = System.Text.Encoding.ASCII.GetBytes("CONNECT " + guestPort + "\n");
        using var writeCts = new CancellationTokenSource(_timeout);
        await _stream.WriteAsync(command, writeCts.Token).ConfigureAwait(false);
        // 读到换行为止（一次 ReadAsync 只保证"有字节"，"OK 200\n" 可能被
        // 拆成多次到达）；行上限 256（host.py 同款——失控 relay 不能逐
        // 字节撑内存）。
        var preamble = new System.Text.StringBuilder();
        var byteBuf = new byte[1];
        while (true)
        {
            var read = await _stream.ReadAsync(byteBuf, cts.Token).ConfigureAwait(false);
            if (read <= 0)
            {
                throw new TransportException("vsock relay closed during handshake");
            }
            if (byteBuf[0] == (byte)'\n')
            {
                break;
            }
            preamble.Append((char)byteBuf[0]);
            if (preamble.Length > 256)
            {
                throw new TransportException("vsock relay preamble too long");
            }
        }
        var reply = preamble.ToString();
        if (!reply.StartsWith("OK ", StringComparison.Ordinal))
        {
            throw new TransportException($"vsock relay rejected: {reply.Trim()}");
        }
    }
#endif

    /// <summary>
    /// Mandatory Hello → HelloAck handshake (PROTOCOL.md §3.4). Any failure —
    /// I/O, a reply that is not a valid HelloAck, or a stale frame from an old
    /// turn — is raised as a TransportException and resets the connection.
    /// </summary>
    private async Task HandshakeAsync()
    {
        ZbrtFrame reply;
        try
        {
            reply = await RoundTripAsync(
                ZbrtFrameCodec.HelloFrame(NewRequestId(), HelloClientName, V1Capabilities))
                .ConfigureAwait(false);
            _ = ZbrtFrameCodec.DecodeHelloAck(reply.Payload);
        }
        catch (TransportException)
        {
            ResetConnection();
            throw;
        }
        catch (RfbException e)
        {
            // Decode-class handshake failures (bad payload, id mismatch) mean
            // the peer cannot speak the handshake either → transport class.
            ResetConnection();
            throw new TransportException($"guest hello failed: {e.Message}", e);
        }

        if (reply.Kind != ZbrtKind.HelloAck)
        {
            ResetConnection();
            throw new TransportException(
                $"guest hello failed: unexpected frame kind {reply.Kind} (expected HelloAck)");
        }
    }

    /// <summary>
    /// Explicit Hello → HelloAck round trip (ConnectAsync already performs the
    /// mandatory handshake with the SDK client name; this sends another Hello,
    /// which the guest answers like any request).
    /// </summary>
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
        return await ExecuteTurnAsync(argv, cwd, stdin, timeoutMs).ConfigureAwait(false);
    }

    /// <summary>长静默 exec 的读预算加成（rust EXEC_READ_MARGIN 同款）：
    /// 逐帧读预算必须盖住 guest 自己的死线，否则合法的长命令先撞客户端
    /// 读超时——命令还在 guest 里跑，客户端却已报错。</summary>
    internal static readonly TimeSpan ExecReadMargin = TimeSpan.FromSeconds(5);

    private async Task<ZbrtExecOutcome> ExecuteTurnAsync(IReadOnlyList<string> argv, string? cwd, byte[] stdin, uint timeoutMs)
    {
        var payload = ZbrtFrameCodec.EncodeExecute(argv, cwd, stdin, timeoutMs);
        var request = new ZbrtFrame { Kind = ZbrtKind.Execute, RequestId = NewRequestId(), Payload = payload };
        var readBudget = _timeout + TimeSpan.FromMilliseconds(timeoutMs) + ExecReadMargin;
        try
        {
            await WriteFrameAsync(request).ConfigureAwait(false);

            var stdout = new List<byte>();
            var stderr = new List<byte>();
            var totalOutput = 0L;
            while (true)
            {
                var frame = await ReadFrameAsync(readBudget).ConfigureAwait(false);
                RequireRequestId(frame, request.RequestId);
                switch (frame.Kind)
                {
                    case ZbrtKind.Output:
                        {
                            var (stream, data) = ZbrtFrameCodec.DecodeOutput(frame.Payload);
                            totalOutput += data.Length;
                            if (totalOutput > MaxTurnOutputBytes)
                            {
                                throw new RemoteException("guest output exceeded the 16 MiB limit");
                            }

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
        catch (Exception e) when (e is TransportException or RemoteException or DecodeException)
        {
            // An aborted turn (timed-out read, remote error, oversized output,
            // desync) leaves unread frames buffered on the socket: drop it so
            // the next request cannot consume stale frames.
            ResetConnection();
            throw;
        }
    }

    /// <summary>Health → HealthAck; returns the guest's healthy flag. The
    /// connection is pool-private for the call's duration.</summary>
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

    /// <summary>借出验活：重发一次 Hello（幂等），失败 = 连接已死。</summary>
    internal async Task HelloProbeAsync()
    {
        await HelloAsync(HelloClientName, V1Capabilities).ConfigureAwait(false);
    }

    /// <summary>Route one filesystem RPC: Fs frame → FsResult JSON (or Error frame).
    /// The connection is pool-private for the call's duration.</summary>
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
        _udsSocket?.Dispose();
        _udsSocket = null;
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

    internal async Task<ZbrtFrame> ReadFrameAsync(TimeSpan? readBudget = null)
    {
        using var cts = new CancellationTokenSource(readBudget ?? _timeout);
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

    /// <summary>rust zbrt.rs 的连接死亡白名单：这些读侧错误才允许在新连接
    /// 上重试一次（对端已消失，功能正常的 guest 不可能再应答）。EPIPE 在
    /// .NET 里以 ConnectionReset/ConnectionAborted 浮现，无独立枚举。</summary>
    private static bool IsConnectionDeath(SocketError code) =>
        code == SocketError.ConnectionReset
        || code == SocketError.ConnectionAborted
        || code == SocketError.NotConnected;

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
            catch (SocketException e) when (IsConnectionDeath(e.SocketErrorCode))
            {
                // 连接死亡类读失败：rust zbrt.rs 白名单（reset/aborted/
                // broken-pipe/not-connected）——可在新连接上重试一次。
                throw new TransportException($"guest read failed: {e.Message}");
            }
            catch (SocketException e)
            {
                // 其它读侧 IO 错误：请求已送达，结果未知——与读超时同等
                // 对待，绝不重试。
                throw new TransportException($"guest read error: {e.Message}");
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
        _udsSocket?.Dispose();
    }
}

/// <summary>统一温连接池（rust zbrt.rs / rfb-ben 的容量形态）：每条连接同一
/// 时刻承载一个操作、可顺序复用；空闲 >1s 的连接在借出时重发 Hello 验活；
/// 并发 = 池中多条连接各服务一个操作（池深 8）。写失败/连接类读失败（请求
/// 未送达）换新连接重试一次；读超时与解码/guest 错误绝不重试。</summary>
internal sealed class ZbrtPool
{
    private readonly string _address;
    private readonly TimeSpan _timeout;
    private readonly System.Collections.Concurrent.ConcurrentQueue<
        (ZbrtTcpClient Client, long LastUsed)> _pool = new();

    internal ZbrtPool(string address, TimeSpan timeout)
    {
        _address = address;
        _timeout = timeout;
    }

    private async Task<ZbrtTcpClient> BorrowAsync()
    {
        while (_pool.TryDequeue(out var entry))
        {
            var (client, lastUsed) = entry;
            // 热路径（背靠背操作）零额外 RTT：只对空闲 >1s 的连接验活。
            if (System.Diagnostics.Stopwatch.GetTimestamp() - lastUsed
                < System.Diagnostics.Stopwatch.Frequency)
            {
                return client;
            }
            try
            {
                await client.HelloProbeAsync().ConfigureAwait(false);
                return client;
            }
            catch (RfbException)
            {
                // Transport 与 Decode 都算验活失败：坏 magic/id 错位的陈旧
                // 连接必须 Dispose 并继续找，异常冲出去会绕过 Run 的换新
                // 连接逻辑。
                client.Dispose();
            }
        }

        var fresh = new ZbrtTcpClient(_address, _timeout);
        await fresh.ConnectAsync().ConfigureAwait(false);
        return fresh;
    }

    private void Repay(ZbrtTcpClient client)
    {
        if (_pool.Count < 8)
        {
            _pool.Enqueue((client, System.Diagnostics.Stopwatch.GetTimestamp()));
        }
        else
        {
            client.Dispose();
        }
    }

    /// <summary>丢弃所有空闲连接（sandbox 删除/停机）。</summary>
    internal void Drain()
    {
        while (_pool.TryDequeue(out var entry))
        {
            entry.Client.Dispose();
        }
    }

    /// <summary>Run one request/response exchange over a borrowed connection.</summary>
    internal async Task<T> Run<T>(Func<ZbrtTcpClient, Task<T>> op)
    {
        var client = await BorrowAsync().ConfigureAwait(false);
        try
        {
            var result = await op(client).ConfigureAwait(false);
            Repay(client);
            return result;
        }
        catch (TransportException e)
        {
            client.Dispose();
            // 写失败 / 连接类读失败 = 请求未送达：换新连接重试一次；
            // 读超时（"guest response timeout"）与解码/guest 错误绝不重试。
            var retryable = e.Message.StartsWith("guest write failed", StringComparison.Ordinal)
                || e.Message.StartsWith("guest read failed", StringComparison.Ordinal)
                || e.Message == "guest closed connection";
            if (!retryable)
            {
                throw;
            }

            var fresh = new ZbrtTcpClient(_address, _timeout);
            try
            {
                await fresh.ConnectAsync().ConfigureAwait(false);
                var result = await op(fresh).ConfigureAwait(false);
                Repay(fresh);
                return result;
            }
            catch
            {
                fresh.Dispose();
                throw;
            }
        }
        catch
        {
            client.Dispose();
            throw;
        }
    }

    /// <summary>Stream 会话独占借出（不归还——一连接一 turn，会话结束关闭）。</summary>
    internal async Task<ZbrtStreamSession> BorrowStreamAsync(
        IReadOnlyList<string> argv, string? cwd)
    {
        var client = await BorrowAsync().ConfigureAwait(false);
        try
        {
            return await client.StreamAsync(argv, cwd).ConfigureAwait(false);
        }
        catch
        {
            client.Dispose();
            throw;
        }
    }
}

/// <summary>One ZBRT stream session: Output frames → events, terminal Exit, targeted Cancel on stop.</summary>
internal sealed class ZbrtStreamSession : IDisposable
{
    private readonly ZbrtTcpClient _client;
    private readonly byte[] _requestId;
    private readonly Queue<StreamEvent> _pending = new();
    private bool _terminal;
    private bool _stopped;

    internal ZbrtStreamSession(ZbrtTcpClient client, byte[] requestId)
    {
        _client = client;
        _requestId = requestId;
    }

    /// <summary>Next stream event; Exit ends the session (subsequent calls return null).
    /// Events buffered during a pending StopAsync are drained first.</summary>
    public async Task<StreamEvent?> NextEventAsync()
    {
        if (_pending.Count > 0)
        {
            return _pending.Dequeue();
        }

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
                        _terminal = true;
                        throw new RemoteException(message);
                    }
                case ZbrtKind.CancelAck:
                    continue;
                default:
                    // 未知帧 = 流已失步：静默顺延会把 desync 无限放大
                    //（java 同款抛 DecodeError）。
                    _terminal = true;
                    throw new DecodeException(
                        $"unexpected frame kind {frame.Kind} while streaming");
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
    /// and await the empty CancelAck. Output frames that arrive between the
    /// Cancel and the CancelAck are buffered and delivered by the next
    /// NextEventAsync calls (never dropped); an Exit frame marks the turn
    /// terminal the same way — mirroring the Python baseline.</summary>
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
                _terminal = true;
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

            if (frame.Kind == ZbrtKind.Output)
            {
                // Buffer straggler output that arrives before the ack.
                var (stream, data) = ZbrtFrameCodec.DecodeOutput(frame.Payload);
                if (stream == 0)
                {
                    _pending.Enqueue(new StreamEvent(StreamEventKind.Stdout, data, null));
                }
                else if (stream == 1)
                {
                    _pending.Enqueue(new StreamEvent(StreamEventKind.Stderr, data, null));
                }
                else
                {
                    throw new DecodeException("invalid output stream");
                }

                continue;
            }

            if (frame.Kind == ZbrtKind.Exit)
            {
                // The turn already terminated: cancel is trivially complete.
                var (code, _) = ZbrtFrameCodec.DecodeExit(frame.Payload);
                _terminal = true;
                _pending.Enqueue(new StreamEvent(StreamEventKind.Exit, [], code));
                return;
            }

            throw new DecodeException($"expected CancelAck, got frame kind {(int)frame.Kind}");
        }
    }

    /// <summary>
    /// Release the underlying connection (idempotent). A stream occupies the
    /// client's single turn, so after disposal the socket state is unknown;
    /// closing it makes the next operation reconnect cleanly.
    /// </summary>
    public void Dispose() => _client.Dispose();
}
