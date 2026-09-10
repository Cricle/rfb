using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Text.Json;
using Rfb.Sdk.Internal;

namespace Rfb.Sdk.Tests;

internal static class Hex
{
    public static byte[] ToBytes(string s) => Convert.FromHexString(s);
    public static string Of(byte[] bytes) => Convert.ToHexString(bytes).ToLowerInvariant();
}

/// <summary>Minimal hand-rolled HTTP/1.1 server over TcpListener (one request per connection).</summary>
internal sealed class FakeHttpServer : IDisposable
{
    public sealed record Request(string Method, string Path, Dictionary<string, string> Headers, string Body);
    public sealed record Response(int Status, string Body);

    private readonly TcpListener _listener;
    private readonly CancellationTokenSource _cts = new();

    public FakeHttpServer(Func<Request, Response> handler)
    {
        Handler = handler;
        _listener = new TcpListener(IPAddress.Loopback, 0);
        _listener.Start();
        Port = ((IPEndPoint)_listener.LocalEndpoint).Port;
        _ = Task.Run(AcceptLoopAsync);
    }

    public Func<Request, Response> Handler { get; set; }
    public int Port { get; }
    public string Url => $"http://127.0.0.1:{Port}";

    private async Task AcceptLoopAsync()
    {
        while (!_cts.IsCancellationRequested)
        {
            TcpClient client;
            try
            {
                client = await _listener.AcceptTcpClientAsync(_cts.Token);
            }
            catch (OperationCanceledException)
            {
                return;
            }
            catch (ObjectDisposedException)
            {
                return;
            }

            try
            {
                Handle(client);
            }
            catch (Exception) when (!_cts.IsCancellationRequested)
            {
                // keep serving subsequent requests
            }
            finally
            {
                client.Dispose();
            }
        }
    }

    private void Handle(TcpClient client)
    {
        using var stream = client.GetStream();
        var head = new MemoryStream();
        var chunk = new byte[4096];
        int headerEnd;
        while (true)
        {
            var n = stream.Read(chunk, 0, chunk.Length);
            if (n == 0)
            {
                return;
            }

            head.Write(chunk, 0, n);
            var bytes = head.ToArray();
            headerEnd = FindHeaderEnd(bytes);
            if (headerEnd >= 0)
            {
                break;
            }
        }

        var raw = head.ToArray();
        var headText = Encoding.ASCII.GetString(raw, 0, headerEnd);
        var lines = headText.Split("\r\n");
        var requestLine = lines[0].Split(' ');
        var method = requestLine[0];
        var path = requestLine[1];
        var headers = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        foreach (var line in lines.Skip(1))
        {
            var sep = line.IndexOf(':');
            if (sep > 0)
            {
                headers[line[..sep].Trim()] = line[(sep + 1)..].Trim();
            }
        }

        var contentLength = headers.TryGetValue("Content-Length", out var cl) ? int.Parse(cl) : 0;
        var bodyBytes = raw[(headerEnd + 4)..];
        while (bodyBytes.Length < contentLength)
        {
            var n = stream.Read(chunk, 0, Math.Min(chunk.Length, contentLength - bodyBytes.Length));
            if (n == 0)
            {
                break;
            }

            head.Write(chunk, 0, n);
            bodyBytes = head.ToArray()[(headerEnd + 4)..];
        }

        var body = Encoding.UTF8.GetString(bodyBytes, 0, Math.Min(bodyBytes.Length, contentLength));
        var response = Handler(new Request(method, path, headers, body));
        var bodyBytesOut = Encoding.UTF8.GetBytes(response.Body);
        var statusText = response.Status switch
        {
            200 => "OK",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Status",
        };
        var responseText =
            $"HTTP/1.1 {response.Status} {statusText}\r\n" +
            "Content-Type: application/json\r\n" +
            $"Content-Length: {bodyBytesOut.Length}\r\n" +
            "Connection: close\r\n\r\n";
        var outBytes = Encoding.ASCII.GetBytes(responseText);
        stream.Write(outBytes, 0, outBytes.Length);
        stream.Write(bodyBytesOut, 0, bodyBytesOut.Length);
        stream.Flush();
    }

    private static int FindHeaderEnd(byte[] bytes)
    {
        for (var i = 0; i + 3 < bytes.Length; i++)
        {
            if (bytes[i] == 13 && bytes[i + 1] == 10 && bytes[i + 2] == 13 && bytes[i + 3] == 10)
            {
                return i;
            }
        }

        return -1;
    }

    public void Dispose()
    {
        _cts.Cancel();
        try
        {
            _listener.Stop();
        }
        catch
        {
            // ignore
        }
    }
}

/// <summary>
/// HTTP/1.1 keep-alive server over TcpListener: serves multiple requests per
/// connection and counts accepts, for TCP-connection-reuse regression tests.
/// </summary>
internal sealed class FakeKeepAliveHttpServer : IDisposable
{
    private readonly TcpListener _listener;
    private readonly CancellationTokenSource _cts = new();

    public FakeKeepAliveHttpServer(Func<FakeHttpServer.Request, FakeHttpServer.Response> handler)
    {
        Handler = handler;
        _listener = new TcpListener(IPAddress.Loopback, 0);
        _listener.Start();
        Port = ((IPEndPoint)_listener.LocalEndpoint).Port;
        _ = Task.Run(AcceptLoopAsync);
    }

    public Func<FakeHttpServer.Request, FakeHttpServer.Response> Handler { get; set; }
    public int Port { get; }
    public string Url => $"http://127.0.0.1:{Port}";

    private int _accepts;

    /// <summary>Number of TCP connections accepted so far.</summary>
    public int AcceptCount => _accepts;

    private async Task AcceptLoopAsync()
    {
        while (!_cts.IsCancellationRequested)
        {
            TcpClient client;
            try
            {
                client = await _listener.AcceptTcpClientAsync(_cts.Token);
            }
            catch (Exception)
            {
                return;
            }

            Interlocked.Increment(ref _accepts);
            _ = Task.Run(async () =>            {
                try
                {
                    await ServeAsync(client);
                }
                catch (Exception) when (!_cts.IsCancellationRequested)
                {
                    // keep the listener alive
                }
                finally
                {
                    client.Dispose();
                }
            });
        }
    }

    private async Task ServeAsync(TcpClient client)
    {
        await using var stream = client.GetStream();
        var head = new MemoryStream();
        var chunk = new byte[4096];
        while (true)
        {
            int headerEnd;
            while (true)
            {
                var raw = head.ToArray();
                headerEnd = FindHeaderEnd(raw);
                if (headerEnd >= 0)
                {
                    break;
                }

                var n = await stream.ReadAsync(chunk);
                if (n == 0)
                {
                    return;
                }

                head.Write(chunk, 0, n);
            }

            var raw2 = head.ToArray();
            var headText = Encoding.ASCII.GetString(raw2, 0, headerEnd);
            var lines = headText.Split("\r\n");
            var requestLine = lines[0].Split(' ');
            var headers = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
            foreach (var line in lines.Skip(1))
            {
                var sep = line.IndexOf(':');
                if (sep > 0)
                {
                    headers[line[..sep].Trim()] = line[(sep + 1)..].Trim();
                }
            }

            var contentLength = headers.TryGetValue("Content-Length", out var cl) ? int.Parse(cl) : 0;
            var total = headerEnd + 4 + contentLength;
            while (head.Length < total)
            {
                var n = await stream.ReadAsync(chunk);
                if (n == 0)
                {
                    return;
                }

                head.Write(chunk, 0, n);
            }

            var body = Encoding.UTF8.GetString(raw2, headerEnd + 4, contentLength);
            var response = Handler(new FakeHttpServer.Request(requestLine[0], requestLine[1], headers, body));
            var bodyBytes = Encoding.UTF8.GetBytes(response.Body);
            var statusText = response.Status switch
            {
                200 => "OK",
                404 => "Not Found",
                500 => "Internal Server Error",
                _ => "Status",
            };
            var headBytes = Encoding.ASCII.GetBytes(
                $"HTTP/1.1 {response.Status} {statusText}\r\n" +
                "Content-Type: application/json\r\n" +
                $"Content-Length: {bodyBytes.Length}\r\n\r\n");
            await stream.WriteAsync(headBytes);
            await stream.WriteAsync(bodyBytes);
            await stream.FlushAsync();

            // consume the served request from the buffer, keep the rest (pipelining)
            var rest = raw2.Length - total;
            var remainder = rest > 0 ? raw2[total..] : [];
            head.SetLength(0);
            head.Write(remainder, 0, remainder.Length);
        }
    }

    private static int FindHeaderEnd(byte[] bytes)
    {
        for (var i = 0; i + 3 < bytes.Length; i++)
        {
            if (bytes[i] == 13 && bytes[i + 1] == 10 && bytes[i + 2] == 13 && bytes[i + 3] == 10)
            {
                return i;
            }
        }

        return -1;
    }

    public void Dispose()
    {
        _cts.Cancel();
        try
        {
            _listener.Stop();
        }
        catch
        {
            // ignore
        }
    }
}

/// <summary>One NDJSON connection session handed to the test handler.</summary>
internal sealed class FakeNdjsonSession
{
    private readonly TcpClient _tcp;
    private readonly NetworkStream _stream;
    private readonly MemoryStream _buffer = new();

    internal FakeNdjsonSession(TcpClient tcp)
    {
        _tcp = tcp;
        _stream = tcp.GetStream();
    }

    public async Task<string?> ReadLineAsync()
    {
        var chunk = new byte[4096];
        while (true)
        {
            var bytes = _buffer.ToArray();
            var nl = Array.IndexOf(bytes, (byte)'\n');
            if (nl >= 0)
            {
                var rest = new MemoryStream();
                rest.Write(bytes, nl + 1, bytes.Length - nl - 1);
                _buffer.SetLength(0);
                _buffer.Write(rest.ToArray(), 0, (int)rest.Length);
                var line = Encoding.UTF8.GetString(bytes, 0, nl).TrimEnd('\r');
                if (line.Length > 0)
                {
                    return line;
                }

                continue; // skip empty keepalive lines
            }

            var n = await _stream.ReadAsync(chunk);
            if (n == 0)
            {
                return null;
            }

            _buffer.Write(chunk, 0, n);
        }
    }

    public async Task WriteAsync(object value)
    {
        var bytes = Encoding.UTF8.GetBytes(JsonSerializer.Serialize(value) + "\n");
        await _stream.WriteAsync(bytes);
        await _stream.FlushAsync();
    }

    /// <summary>
    /// Write multiple NDJSON lines in ONE TCP write (deterministic coalescing —
    /// Linux loopback regularly merges separate writes into one segment, which
    /// is exactly what a stateful client reader must survive).
    /// </summary>
    public async Task WriteLinesAsync(params object[] values)
    {
        var ms = new MemoryStream();
        foreach (var value in values)
        {
            var bytes = Encoding.UTF8.GetBytes(JsonSerializer.Serialize(value) + "\n");
            ms.Write(bytes, 0, bytes.Length);
        }

        await _stream.WriteAsync(ms.ToArray());
        await _stream.FlushAsync();
    }

    public void Close() => _tcp.Dispose();
}

/// <summary>
/// Fake forkd guest speaking newline-delimited JSON over TCP. The default handler
/// implements ping/exec/eval/ls/find/grep/read/write/stream per PROTOCOL.md §2.
/// </summary>
internal sealed class FakeNdjsonGuest : IDisposable
{
    private readonly TcpListener _listener;
    private readonly CancellationTokenSource _cts = new();

    /// <summary>Number of TCP connections accepted (for connection-count regression tests).</summary>
    private int _acceptCount;
    public int AcceptCount => _acceptCount;

    public FakeNdjsonGuest()
    {
        _listener = new TcpListener(IPAddress.Loopback, 0);
        _listener.Start();
        Port = ((IPEndPoint)_listener.LocalEndpoint).Port;
        _ = Task.Run(AcceptLoopAsync);
    }

    public Func<FakeNdjsonSession, JsonElement, Task> Handler { get; set; } = DefaultHandler;

    public int Port { get; }
    public string Address => $"127.0.0.1:{Port}";

    private async Task AcceptLoopAsync()
    {
        while (!_cts.IsCancellationRequested)
        {
            TcpClient client;
            try
            {
                client = await _listener.AcceptTcpClientAsync(_cts.Token);
            }
            catch (Exception)
            {
                return;
            }

            Interlocked.Increment(ref _acceptCount);

            var session = new FakeNdjsonSession(client);
            _ = Task.Run(async () =>
            {
                try
                {
                    while (true)
                    {
                        var line = await session.ReadLineAsync();
                        if (line is null)
                        {
                            return;
                        }

                        var value = JsonDocument.Parse(line).RootElement;
                        await Handler(session, value);
                    }
                }
                catch (Exception e) when (!_cts.IsCancellationRequested)
                {
                    Console.Error.WriteLine($"[FakeNdjsonGuest] session died: {e}");
                    // connection-level failures end the session quietly
                }
            });
        }
    }

    public static Task Write(FakeNdjsonSession s, object value) => s.WriteAsync(value);

    public static async Task DefaultHandler(FakeNdjsonSession session, JsonElement value)
    {
        var action = value.TryGetProperty("action", out var a) ? a.GetString() : null;
        Console.Error.WriteLine($"[FakeNdjsonGuest] handler entry: {action}");
        switch (action)
        {
            case "ping":
                await session.WriteAsync(new { pong = true });
                break;
            case "exec":
                {
                    var args = value.GetProperty("args").EnumerateArray().Select(x => x.GetString()!).ToList();
                    if (args[0] == "fail")
                    {
                        await session.WriteAsync(new { error = "exec failed: boom" });
                        return;
                    }

                    if (args[0] == "slowfail")
                    {
                        await session.WriteAsync(new { progress = 1 });
                        await session.WriteAsync(new { error = "exec failed: late" });
                        return;
                    }

                    await session.WriteAsync(new { progress = 1 });
                    Console.Error.WriteLine("[FakeNdjsonGuest] exec: progress written");
                    await session.WriteAsync(new Dictionary<string, object?>
                    {
                        ["exit_code"] = 0,
                        ["out"] = "hi\n",
                        ["err"] = "e\n",
                        ["timed_out"] = false,
                    });
                    Console.Error.WriteLine("[FakeNdjsonGuest] exec: exit written");
                    break;
                }
            case "eval":
                await session.WriteAsync(new Dictionary<string, object?> {
                    ["output"] = new List<object> { 0x32 }, ["status"] = 0, ["timed_out"] = false,
                });
                break;
            case "ls":
                await session.WriteAsync(new
                {
                    entries = new object[] {
                        new { name = "a.txt", is_dir = false, size = 3 },
                        new { name = "sub", is_dir = true },
                    },
                    truncated = false,
                });
                break;
            case "find":
                await session.WriteAsync(new { matches = new[] { "a.txt" }, truncated = false });
                break;
            case "grep":
                await session.WriteAsync(new
                {
                    matches = new object[] {
                        new { path = "a.txt", line = 1, column = 1, text = "hi" },
                        new { path = "b.txt", text = "hm" },
                    },
                    truncated = false,
                });
                break;
            case "read":
                await session.WriteAsync(new Dictionary<string, object?> {
                    ["data"] = new List<object> { 104, 105 }, ["truncated"] = false,
                });
                break;
            case "write":
                await session.WriteAsync(new { bytes_written = 2 });
                break;
            case "stream":
                {
                    await session.WriteAsync(new { started = true });
                    while (true)
                    {
                        var line = await session.ReadLineAsync();
                        if (line is null)
                        {
                            return;
                        }

                        var next = JsonDocument.Parse(line).RootElement;
                        if (next.TryGetProperty("in", out var input))
                        {
                            await session.WriteAsync(new { stdout = $"echo:{input.GetString()}" });
                        }
                        else if (next.TryGetProperty("action", out var stop) && stop.GetString() == "stop")
                        {
                            await session.WriteAsync(new { exit_code = 9 });
                            return;
                        }
                    }
                }
            default:
                await session.WriteAsync(new { error = $"unknown action {action}" });
                break;
        }
    }

    public void Dispose()
    {
        _cts.Cancel();
        try
        {
            _listener.Stop();
        }
        catch
        {
            // ignore
        }
    }
}

/// <summary>
/// Fake ZBRT v1 frame server over TCP. Execute streams Output frames then one
/// terminal Exit; Cancel → CancelAck + Exit(-1); Health → HealthAck; Fs → FsResult.
/// </summary>
internal sealed class FakeZbrtServer : IDisposable
{
    private readonly TcpListener _listener;
    private readonly CancellationTokenSource _cts = new();

    public FakeZbrtServer()
    {
        _listener = new TcpListener(IPAddress.Loopback, 0);
        _listener.Start();
        Port = ((IPEndPoint)_listener.LocalEndpoint).Port;
        _ = Task.Run(AcceptLoopAsync);
    }

    public int Port { get; }
    public string Address => $"127.0.0.1:{Port}";

    /// <summary>Number of TCP connections accepted (fail-closed validation tests assert 0).</summary>
    private int _acceptCount;
    public int AcceptCount => _acceptCount;

    /// <summary>Reason string of the last received Cancel frame (null = no reason flag).</summary>
    public string? LastCancelReason { get; private set; }

    /// <summary>Last Fs frame op + JSON args (PROTOCOL.md §3.3 null-key conformance).</summary>
    public byte LastFsOp { get; private set; }
    public string? LastFsJson { get; private set; }

    private async Task AcceptLoopAsync()
    {
        while (!_cts.IsCancellationRequested)
        {
            TcpClient client;
            try
            {
                client = await _listener.AcceptTcpClientAsync(_cts.Token);
            }
            catch (Exception)
            {
                return;
            }

            Interlocked.Increment(ref _acceptCount);
            _ = Task.Run(async () =>
            {
                try
                {
                    await ServeAsync(client);
                }
                catch (Exception) when (!_cts.IsCancellationRequested)
                {
                    // ignore per-connection failures
                }
                finally
                {
                    client.Dispose();
                }
            });
        }
    }

    private async Task ServeAsync(TcpClient client)
    {
        await using var stream = client.GetStream();
        while (true)
        {
            ZbrtFrame frame;
            try
            {
                frame = await ReadFrameAsync(stream);
            }
            catch (EndOfStreamException)
            {
                return;
            }

            switch (frame.Kind)
            {
                case ZbrtKind.Execute:
                    {
                        var exec = ZbrtFrameCodec.DecodeExecute(frame.Payload);
                        if (exec.Argv.Count == 0)
                        {
                            await WriteAsync(stream, Error(frame.RequestId, 1, "argv is empty"));
                            continue;
                        }

                        if (exec.Argv[0] == "fail")
                        {
                            await WriteAsync(stream, Error(frame.RequestId, 2, "boom"));
                            continue;
                        }

                        if (exec.Argv[0] == "eval")
                        {
                            await WriteAsync(stream, Output(frame.RequestId, 0, "2"));
                            await WriteAsync(stream, Exit(frame.RequestId, 0));
                            continue;
                        }

                        if (exec.Argv[0] == "cat")
                        {
                            // stream mode: one Output, then hold until Cancel arrives
                            await WriteAsync(stream, Output(frame.RequestId, 0, "hi\n"));
                            continue;
                        }

                        await WriteAsync(stream, Output(frame.RequestId, 0, "hi\n"));
                        await WriteAsync(stream, Output(frame.RequestId, 1, "e\n"));
                        await WriteAsync(stream, Exit(frame.RequestId, 0));
                        break;
                    }
                case ZbrtKind.Health:
                    await WriteAsync(stream, new ZbrtFrame
                    {
                        Kind = ZbrtKind.HealthAck,
                        RequestId = frame.RequestId,
                        Payload = ZbrtFrameCodec.EncodeHealth(true, "ready"),
                    });
                    break;
                case ZbrtKind.Fs:
                    {
                        var fs = ZbrtFrameCodec.DecodeFs(frame.Payload);
                        LastFsOp = fs.Op;
                        LastFsJson = Encoding.UTF8.GetString(fs.JsonData);
                        var json = fs.Op switch
                        {
                            1 => """{"entries":[{"name":"a.txt","is_dir":false,"size":3},{"name":"sub","is_dir":true}],"truncated":false}""",
                            2 => """{"matches":["a.txt"],"truncated":false}""",
                            3 => """{"matches":[{"path":"a.txt","line":1,"column":1,"text":"hi"},{"path":"b.txt","text":"hm"}],"truncated":false}""",
                            4 => """{"data":[104,105],"truncated":false}""",
                            5 => """{"bytes_written":2}""",
                            _ => null,
                        };
                        if (json is null)
                        {
                            await WriteAsync(stream, Error(frame.RequestId, 1, $"unsupported fs op {fs.Op}"));
                            continue;
                        }

                        await WriteAsync(stream, new ZbrtFrame
                        {
                            Kind = ZbrtKind.FsResult,
                            RequestId = frame.RequestId,
                            Payload = Encoding.UTF8.GetBytes(json),
                        });
                        break;
                    }
                case ZbrtKind.Cancel:
                    {
                        var cancel = ZbrtFrameCodec.DecodeCancel(frame.Payload);
                        LastCancelReason = cancel.Reason;
                        await WriteAsync(stream, new ZbrtFrame
                        {
                            Kind = ZbrtKind.CancelAck,
                            RequestId = frame.RequestId,
                            Payload = [],
                        });
                        var target = cancel.Target ?? frame.RequestId;
                        await WriteAsync(stream, Exit(target, -1));
                        break;
                    }
                case ZbrtKind.Hello:
                    await WriteAsync(stream, new ZbrtFrame
                    {
                        Kind = ZbrtKind.HelloAck,
                        RequestId = frame.RequestId,
                        Payload = ZbrtFrameCodec.EncodeHelloAck("rfb-zeroboot-guest", new[] {
                            "execute", "stream", "deadline", "health", "cancel", "filesystem",
                        }),
                    });
                    break;
                default:
                    await WriteAsync(stream, Error(frame.RequestId, 1, "unsupported request kind"));
                    break;
            }
        }
    }

    private static ZbrtFrame Output(byte[] requestId, byte stream, string data) =>
        new()
        {
            Kind = ZbrtKind.Output,
            RequestId = requestId,
            Payload = ZbrtFrameCodec.EncodeOutput(stream, Encoding.UTF8.GetBytes(data)),
        };

    private static ZbrtFrame Exit(byte[] requestId, int code) =>
        new()
        {
            Kind = ZbrtKind.Exit,
            RequestId = requestId,
            Payload = ZbrtFrameCodec.EncodeExit(code, null),
        };

    private static ZbrtFrame Error(byte[] requestId, uint code, string message) =>
        new()
        {
            Kind = ZbrtKind.Error,
            RequestId = requestId,
            Payload = ZbrtFrameCodec.EncodeError(code, message),
        };

    private static async Task WriteAsync(NetworkStream stream, ZbrtFrame frame)
    {
        var bytes = ZbrtFrameCodec.Encode(frame);
        await stream.WriteAsync(bytes);
        await stream.FlushAsync();
    }

    private static async Task<ZbrtFrame> ReadFrameAsync(NetworkStream stream)
    {
        var header = new byte[ZbrtFrameCodec.HeaderLen];
        await ReadExactlyAsync(stream, header);
        var len = System.Buffers.Binary.BinaryPrimitives.ReadUInt32BigEndian(header.AsSpan(24, 4));
        var payload = new byte[len];
        await ReadExactlyAsync(stream, payload);
        var full = new byte[header.Length + payload.Length];
        header.CopyTo(full, 0);
        payload.CopyTo(full, header.Length);
        return ZbrtFrameCodec.Decode(full);
    }

    private static async Task ReadExactlyAsync(NetworkStream stream, byte[] buffer)
    {
        var offset = 0;
        while (offset < buffer.Length)
        {
            var n = await stream.ReadAsync(buffer.AsMemory(offset));
            if (n == 0)
            {
                throw new EndOfStreamException();
            }

            offset += n;
        }
    }

    public void Dispose()
    {
        _cts.Cancel();
        try
        {
            _listener.Stop();
        }
        catch
        {
            // ignore
        }
    }
}
