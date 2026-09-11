using System.Net.Sockets;
using System.Text;
using System.Text.Json;

namespace Rfb.Sdk.Internal;

/// <summary>
/// TCP client for a forkd guest speaking newline-delimited JSON (PROTOCOL.md §2;
/// mirror of rfb/src/forkd/guest.rs). Internal only — never part of the public API.
/// </summary>
internal sealed class ForkdGuestNdjson
{
    public const int MaxLineBytes = 1024 * 1024;

    private readonly string _host;
    private readonly int _port;
    private readonly TimeSpan _timeout;

    public ForkdGuestNdjson(string address, TimeSpan timeout)
    {
        (_host, _port) = WireJson.ParseGuestAddress(address);
        _timeout = timeout;
    }

    public async Task<JsonElement> PingAsync() =>
        await LastResponseAsync(new Dictionary<string, object?> { ["action"] = "ping" }).ConfigureAwait(false);

    public async Task<JsonElement> ExecAsync(string cwd, IReadOnlyList<string> args, ulong timeoutSecs) =>
        await LastResponseAsync(new Dictionary<string, object?>
        {
            ["action"] = "exec",
            ["cwd"] = cwd,
            ["args"] = args,
            ["timeout"] = timeoutSecs,
        });

    public async Task<JsonElement> EvalAsync(string code, string? cwd, double? timeoutS)
    {
        var action = new Dictionary<string, object?>
        {
            ["action"] = "eval",
            ["code"] = code,
        };
        if (cwd is not null)
        {
            action["cwd"] = cwd;
        }

        if (timeoutS.HasValue)
        {
            action["timeout"] = TimeoutSecs(timeoutS.Value);
        }

        return await LastResponseAsync(action).ConfigureAwait(false);
    }

    public async Task<JsonElement> ToolAsync(string tool, Dictionary<string, object?> args)
    {
        args["action"] = tool;
        return await LastResponseAsync(args).ConfigureAwait(false);
    }

    public async Task<ForkdGuestNdjsonStream> StreamAsync(
        IReadOnlyList<string> args, string? cwd, bool? pty, IReadOnlyDictionary<string, object?>? env)
    {
        var tcp = new TcpClient();
        try
        {
            await ConnectAsync(tcp).ConfigureAwait(false);
            var stream = tcp.GetStream();
            var action = new Dictionary<string, object?>
            {
                ["action"] = "stream",
                ["args"] = args,
            };
            if (cwd is not null)
            {
                action["cwd"] = cwd;
            }

            if (pty.HasValue)
            {
                action["pty"] = pty.Value;
            }

            if (env is not null)
            {
                action["env"] = env;
            }

            await WriteLineAsync(stream, JsonSerializer.Serialize(action)).ConfigureAwait(false);
            return new ForkdGuestNdjsonStream(tcp, stream, _timeout);
        }
        catch
        {
            tcp.Dispose();
            throw;
        }
    }

    /// <summary>Raw request exposed for tests: all response lines up to and including the terminal one.</summary>
    internal Task<List<JsonElement>> RequestAsyncForTests(Dictionary<string, object?> action) => RequestAsync(action);

    private async Task<JsonElement> LastResponseAsync(Dictionary<string, object?> action)
    {
        var responses = await RequestAsync(action).ConfigureAwait(false);
        return responses[^1];
    }

    /// <summary>Send one action and read response lines until the terminal line.</summary>
    private async Task<List<JsonElement>> RequestAsync(Dictionary<string, object?> action)
    {
        using var tcp = new TcpClient();
        await ConnectAsync(tcp).ConfigureAwait(false);
        using var stream = tcp.GetStream();
        await WriteLineAsync(stream, JsonSerializer.Serialize(action)).ConfigureAwait(false);

        var reader = new NdjsonLineReader(stream, _timeout);
        var responses = new List<JsonElement>();
        while (true)
        {
            // Skip empty keepalive lines like the Rust/Python/Java baselines.
            var line = await reader.ReadLineAsync(skipEmpty: true).ConfigureAwait(false);
            if (line is null)
            {
                throw new RemoteException("guest closed before response");
            }

            JsonElement value;
            try
            {
                value = JsonDocument.Parse(line).RootElement.Clone();
            }
            catch (JsonException e)
            {
                throw new DecodeException($"invalid guest JSON: {e.Message}");
            }

            WireJson.CheckRemoteError(value);
            responses.Add(value);
            if (WireJson.IsTerminalLine(value))
            {
                break;
            }
        }

        return responses;
    }

    private async Task ConnectAsync(TcpClient tcp)
    {
        using var cts = new CancellationTokenSource(_timeout);
        try
        {
            await TcpCompat.ConnectAsync(tcp, _host, _port, cts.Token).ConfigureAwait(false);
            tcp.NoDelay = true;
        }
        catch (OperationCanceledException)
        {
            throw new TransportException("guest connect timeout");
        }
        catch (SocketException e)
        {
            throw new TransportException($"guest connect failed: {e.Message}", e);
        }
    }

    private static async Task WriteLineAsync(NetworkStream stream, string line)
    {
        // Encode into ONE buffer and issue ONE write (no string concat copy).
        var bytes = new byte[Encoding.UTF8.GetByteCount(line) + 1];
        Encoding.UTF8.GetBytes(line).CopyTo(bytes.AsSpan());
        bytes[^1] = (byte)'\n';
        await stream.WriteAsync(bytes, 0, bytes.Length).ConfigureAwait(false);
    }

    /// <summary>RFB durations are milliseconds; forkd's eval/exec timeout is seconds (ceil, min 1).</summary>
    public static ulong TimeoutSecs(double seconds)
    {
        var secs = (ulong)Math.Ceiling(seconds);
        return secs == 0 ? 1 : secs;
    }
}

/// <summary>One stream session over a single NDJSON TCP connection.</summary>
internal sealed class ForkdGuestNdjsonStream : IDisposable
{
    private readonly TcpClient _tcp;
    private readonly NetworkStream _stream;
    private readonly NdjsonLineReader _reader;
    private readonly TimeSpan _timeout; // write timeout
    private bool _stopped;
    private bool _terminal;

    internal ForkdGuestNdjsonStream(TcpClient tcp, NetworkStream stream, TimeSpan timeout)
    {
        _tcp = tcp;
        _stream = stream;
        _reader = new NdjsonLineReader(stream, timeout);
        _timeout = timeout;
    }

    /// <summary>Read the next event line; clean close → null. Terminal exit marks the session ended.</summary>
    public async Task<JsonElement?> NextEventAsync()
    {
        if (_terminal)
        {
            return null; // sequence ended with the terminal exit event
        }

        var line = await _reader.ReadLineAsync(skipEmpty: true).ConfigureAwait(false);
        if (line is null)
        {
            return null;
        }

        JsonElement value;
        try
        {
            value = JsonDocument.Parse(line).RootElement.Clone();
        }
        catch (JsonException e)
        {
            throw new DecodeException($"invalid guest JSON: {e.Message}");
        }

        WireJson.CheckRemoteError(value);
        if (WireJson.HasKey(value, "exit_code"))
        {
            _terminal = true;
        }

        return value;
    }

    public async Task SendInputAsync(string input)
    {
        if (_terminal || _stopped)
        {
            throw new RemoteException("guest stream is no longer running");
        }

        await WriteJsonAsync(JsonSerializer.Serialize(new Dictionary<string, object?> { ["in"] = input })).ConfigureAwait(false);
    }

    public async Task StopAsync()
    {
        if (_terminal || _stopped)
        {
            return; // idempotent
        }

        _stopped = true;
        await WriteJsonAsync(JsonSerializer.Serialize(new Dictionary<string, object?> { ["action"] = "stop" })).ConfigureAwait(false);
    }

    private async Task WriteJsonAsync(string json)
    {
        using var cts = new CancellationTokenSource(_timeout);
        try
        {
            var bytes = new byte[Encoding.UTF8.GetByteCount(json) + 1];
            Encoding.UTF8.GetBytes(json).CopyTo(bytes.AsSpan());
            bytes[^1] = (byte)'\n';
            await _stream.WriteAsync(bytes, 0, bytes.Length, cts.Token).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            throw new TransportException("guest write timeout");
        }
        catch (SocketException e)
        {
            throw new TransportException($"guest write failed: {e.Message}", e);
        }
    }

    public void Dispose()
    {
        _stream.Dispose();
        _tcp.Dispose();
    }
}
