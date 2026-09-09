using System.Text.Json;
using Rfb.Sdk.Internal;

namespace Rfb.Sdk;

/// <summary>
/// Sandbox facade over one forkd guest. Transport is "ndjson" (default) or
/// "zbrt"; method names and result shapes are identical on both (UNIFIED_API.md §4).
/// </summary>
public sealed class Sandbox
{
    private readonly RfbClient _client;
    private readonly TimeSpan _timeout;
    private readonly Lazy<Task<ForkdGuestNdjson>> _ndjson;
    private readonly Lazy<Task<ZbrtTcpClient>> _zbrt;

    internal Sandbox(RfbClient client, SandboxInfo info, string transport, TimeSpan timeout)
    {
        if (transport is not ("ndjson" or "zbrt"))
        {
            throw new ValidationException("transport must be \"ndjson\" or \"zbrt\"");
        }

        _client = client;
        _timeout = timeout;
        Info = info;
        Transport = transport;
        _ndjson = new Lazy<Task<ForkdGuestNdjson>>(() =>
            Task.FromResult(new ForkdGuestNdjson(info.GuestAddr, timeout)));
        _zbrt = new Lazy<Task<ZbrtTcpClient>>(() =>
            Task.FromResult(new ZbrtTcpClient(info.GuestAddr, timeout)));
    }

    public string Id => Info.Id;
    public string SnapshotTag => Info.SnapshotTag;
    public string GuestAddr => Info.GuestAddr;
    public long? CreatedAtUnix => Info.CreatedAtUnix;
    public SandboxInfo Info { get; }
    public string Transport { get; }

    private ForkdGuestNdjson Guest => Transport == "ndjson"
        ? _ndjson.Value.Result
        : throw new InvalidOperationException("ndjson transport not active");

    private ZbrtTcpClient Zbrt => Transport == "zbrt"
        ? _zbrt.Value.Result
        : throw new InvalidOperationException("zbrt transport not active");

    public async Task<bool> Ping()
    {
        if (Transport == "ndjson")
        {
            var v = await Guest.PingAsync();
            return v.ValueKind == JsonValueKind.Object
                && v.TryGetProperty("pong", out var pong)
                && pong.ValueKind == JsonValueKind.True;
        }

        return await Zbrt.HealthAsync();
    }

    public async Task<ExecResult> Exec(IReadOnlyList<string> args, string cwd = "/", double timeoutS = 60.0, byte[]? stdin = null)
    {
        if (args.Count == 0)
        {
            throw new ValidationException("argv is empty");
        }

        GuestValidation.FilePath(cwd);
        GuestValidation.Timeout(timeoutS);

        if (Transport == "ndjson")
        {
            // Rust baseline: the NDJSON exec wire contract has no stdin
            // channel; non-empty stdin is silently dropped (delivered only
            // over ZBRT).
            var v = await Guest.ExecAsync(cwd, args, ForkdGuestNdjson.TimeoutSecs(timeoutS));
            return GuestResults.ParseExec(v);
        }

        var outcome = await Zbrt.ExecuteAsync(args, cwd, stdin ?? [], TimeoutMs(timeoutS));
        return new ExecResult(outcome.ExitCode, outcome.Stdout, outcome.Stderr, false);
    }

    public async Task<ExecResult> Eval(string code, string? cwd = null, double? timeoutS = null)
    {
        GuestValidation.EvalCode(code);
        if (cwd is not null)
        {
            GuestValidation.FilePath(cwd);
        }

        GuestValidation.EvalTimeout(timeoutS);

        if (Transport == "ndjson")
        {
            var v = await Guest.EvalAsync(code, cwd, timeoutS);
            return GuestResults.ParseEval(v);
        }

        var argv = new List<string> { "eval", code };
        var outcome = await Zbrt.ExecuteAsync(argv, cwd, [], TimeoutMs(timeoutS));
        return new ExecResult(outcome.ExitCode, outcome.Stdout, outcome.Stderr, false);
    }

    public async Task<IReadOnlyList<DirEntry>> Ls(string path = ".")
    {
        GuestValidation.FsPath(path);
        if (Transport == "ndjson")
        {
            var v = await Guest.ToolAsync("ls", new Dictionary<string, object?>
            {
                ["path"] = path,
                ["max_results"] = GuestValidation.MaxResults,
            });
            return GuestResults.ParseLs(v);
        }

        var result = await Zbrt.FsAsync(FsOp.Ls, path, LsJson);
        return GuestResults.ParseLs(result);
    }

    /// <summary>Find guest paths whose file name matches <paramref name="pattern"/> (path first, UNIFIED_API.md §4).</summary>
    public Task<IReadOnlyList<string>> Find(string path, string pattern)
    {
        GuestValidation.FsPath(path);
        GuestValidation.Pattern(pattern);
        if (Transport == "ndjson")
        {
            return FindCore(path, pattern);
        }

        return FindZbrt(path, pattern);
    }

    /// <summary>Convenience overload: <c>Find(".", pattern)</c>.</summary>
    public Task<IReadOnlyList<string>> Find(string pattern) => Find(".", pattern);

    private async Task<IReadOnlyList<string>> FindCore(string path, string pattern)
    {
        var v = await Guest.ToolAsync("find", new Dictionary<string, object?>
        {
            ["path"] = path,
            ["pattern"] = pattern,
            ["max_results"] = GuestValidation.MaxResults,
        });
        return GuestResults.ParseFind(v);
    }

    private async Task<IReadOnlyList<string>> FindZbrt(string path, string pattern)
    {
        var json = JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["pattern"] = pattern,
            ["max_results"] = GuestValidation.MaxResults,
        });
        var result = await Zbrt.FsAsync(FsOp.Find, path, json);
        return GuestResults.ParseFind(result);
    }

    /// <summary>Grep guest file contents (path first, UNIFIED_API.md §4).</summary>
    public Task<IReadOnlyList<GrepMatch>> Grep(string path, string pattern)
    {
        GuestValidation.FsPath(path);
        GuestValidation.Pattern(pattern);
        if (Transport == "ndjson")
        {
            return GrepCore(path, pattern);
        }

        return GrepZbrt(path, pattern);
    }

    /// <summary>Convenience overload: <c>Grep(".", pattern)</c>.</summary>
    public Task<IReadOnlyList<GrepMatch>> Grep(string pattern) => Grep(".", pattern);

    private async Task<IReadOnlyList<GrepMatch>> GrepCore(string path, string pattern)
    {
        var v = await Guest.ToolAsync("grep", new Dictionary<string, object?>
        {
            ["path"] = path,
            ["pattern"] = pattern,
            ["max_results"] = GuestValidation.MaxResults,
            ["max_bytes"] = GuestValidation.MaxResultBytes,
        });
        return GuestResults.ParseGrep(v);
    }

    private async Task<IReadOnlyList<GrepMatch>> GrepZbrt(string path, string pattern)
    {
        var json = JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["pattern"] = pattern,
            ["max_results"] = GuestValidation.MaxResults,
            ["max_bytes"] = GuestValidation.MaxResultBytes,
        });
        var result = await Zbrt.FsAsync(FsOp.Grep, path, json);
        return GuestResults.ParseGrep(result);
    }

    public async Task<FileRead> Read(string path, long? offset = null, long? maxBytes = null)
    {
        GuestValidation.FilePath(path);
        if (maxBytes.HasValue)
        {
            GuestValidation.Limit(maxBytes.Value, GuestValidation.MaxResultBytes);
        }

        if (Transport == "ndjson")
        {
            var args = new Dictionary<string, object?> { ["path"] = path };
            if (offset.HasValue)
            {
                args["offset"] = offset.Value;
            }

            if (maxBytes.HasValue)
            {
                args["max_bytes"] = maxBytes.Value;
            }

            var v = await Guest.ToolAsync("read", args);
            return GuestResults.ParseRead(v);
        }

        var json = JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["offset"] = offset,
            ["max_bytes"] = maxBytes,
        });
        var result = await Zbrt.FsAsync(FsOp.Read, path, json);
        return GuestResults.ParseRead(result);
    }

    public async Task<long> Write(string path, byte[] data, bool append = false, uint? mode = null)
    {
        GuestValidation.FilePath(path);
        GuestValidation.PayloadSize(data.Length, GuestValidation.MaxResultBytes);
        if (Transport == "ndjson")
        {
            var args = new Dictionary<string, object?>
            {
                ["path"] = path,
                ["data"] = WireJson.ByteList(data),
                ["append"] = append,
            };
            if (mode.HasValue)
            {
                args["mode"] = mode.Value;
            }

            var v = await Guest.ToolAsync("write", args);
            return GuestResults.ParseWrite(v);
        }

        var json = JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["data"] = WireJson.ByteList(data),
            ["append"] = append,
            ["mode"] = mode,
        });
        var result = await Zbrt.FsAsync(FsOp.Write, path, json);
        return GuestResults.ParseWrite(result);
    }

    /// <summary>Start an interactive stream. env/pty are NDJSON-only options.</summary>
    public async Task<GuestStream> Stream(
        IReadOnlyList<string> args,
        string? cwd = null,
        bool? pty = null,
        IReadOnlyDictionary<string, string>? env = null)
    {
        if (args.Count == 0)
        {
            throw new ValidationException("argv is empty");
        }

        if (cwd is not null)
        {
            GuestValidation.FilePath(cwd);
        }

        if (Transport == "ndjson")
        {
            var envObj = env is null
                ? null
                : (IReadOnlyDictionary<string, object?>)env.ToDictionary(kv => kv.Key, kv => (object?)kv.Value);
            var session = await Guest.StreamAsync(args, cwd, pty, envObj);
            return new GuestStream(session);
        }

        if (pty == true)
        {
            throw new ValidationException("pty is not supported over the ZBRT transport");
        }

        if (env is { Count: > 0 })
        {
            throw new ValidationException("env is not supported over zbrt transport");
        }

        var zbrtSession = await Zbrt.StreamAsync(args, cwd);
        return new GuestStream(zbrtSession);
    }

    /// <summary>Delete this sandbox (2xx/404 both succeed).</summary>
    public async Task Delete()
    {
        await _client.DeleteSandbox(Id);
    }

    // Constant ZBRT fs-request bodies (payload never varies → build once).
    private static readonly byte[] LsJson =
        JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["max_results"] = GuestValidation.MaxResults,
        });

    // ZBRT deadlines are whole seconds ceil-ed (like NDJSON TimeoutSecs), then ×1000.
    private static uint TimeoutMs(double timeoutS)
    {
        var ms = Math.Ceiling(timeoutS) * 1000;
        return ms >= uint.MaxValue ? uint.MaxValue : (uint)ms;
    }

    private static uint TimeoutMs(double? timeoutS) => timeoutS.HasValue ? TimeoutMs(timeoutS.Value) : 0;

    private static class FsOp
    {
        public const byte Ls = 1;
        public const byte Find = 2;
        public const byte Grep = 3;
        public const byte Read = 4;
        public const byte Write = 5;
    }
}
