using System.Text.Json;
using Rfb.Sdk.Internal;

namespace Rfb.Sdk;

/// <summary>
/// Sandbox facade over one forkd guest. Transport is "ndjson" (default) or
/// "zbrt"; method names and result shapes are identical on both (UNIFIED_API.md §4).
/// </summary>
public sealed class Sandbox : IAsyncDisposable
{
    private readonly RfbClient _client;
    private readonly TimeSpan _timeout;
    private readonly Lazy<ForkdGuestNdjson> _ndjson;
    private readonly Lazy<ZbrtPool> _zbrt;

    /// <summary>Attach a sandbox at a KNOWN guest address with an explicit
    /// transport — the entry point for direct ZBRT bridges (no controller
    /// involved). Mirrors the java SDK's <c>Sandbox.attach</c>.</summary>
    /// <param name="client">Owning client (supplies the timeout).</param>
    /// <param name="info">Controller metadata carrying the guest address.</param>
    /// <param name="transport">Guest transport: "ndjson" or "zbrt".</param>
    /// <returns>A sandbox handle bound to the given guest address.</returns>
    /// <exception cref="ValidationException">Invalid transport.</exception>
    public static Sandbox Attach(RfbClient client, SandboxInfo info, string transport)
    {
        if (transport != RfbClient.TransportNdjson && transport != RfbClient.TransportZbrt)
        {
            throw new ValidationException($"invalid transport: {transport}");
        }
        return new Sandbox(client, info, transport,
            TimeSpan.FromSeconds(client.TimeoutS));
    }

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
        // Plain Lazy<T>: the constructors are synchronous, so Task.FromResult
        // wrappers (and the .Result unwrap they forced) added nothing.
        _ndjson = new Lazy<ForkdGuestNdjson>(() => new ForkdGuestNdjson(info.GuestAddr, timeout));
        _zbrt = new Lazy<ZbrtPool>(() => new ZbrtPool(info.GuestAddr, timeout));
    }

    /// <summary>Sandbox id (controller-assigned).</summary>
    public string Id => Info.Id;
    /// <summary>Tag of the snapshot this sandbox was created from.</summary>
    public string SnapshotTag => Info.SnapshotTag;
    /// <summary>Host:port of the guest agent (TCP).</summary>
    public string GuestAddr => Info.GuestAddr;
    /// <summary>Creation time (Unix seconds).</summary>
    public long? CreatedAtUnix => Info.CreatedAtUnix;
    /// <summary>Raw controller metadata for this sandbox.</summary>
    public SandboxInfo Info { get; }
    /// <summary>Guest transport in use: "ndjson" or "zbrt".</summary>
    public string Transport { get; }

    private ForkdGuestNdjson Guest => Transport == "ndjson"
        ? _ndjson.Value
        : throw new InvalidOperationException("ndjson transport not active");

    private ZbrtPool Zbrt => Transport == "zbrt"
        ? _zbrt.Value
        : throw new InvalidOperationException("zbrt transport not active");

    /// <summary>Guest health: true only when the agent answers pong=true
    /// (NDJSON) or reports a healthy HealthAck (ZBRT).</summary>
    /// <returns>True when the guest agent is healthy.</returns>
    /// <exception cref="TransportException">Connection failure or timeout.</exception>
    public async Task<bool> Ping()
    {
        if (Transport == "ndjson")
        {
            var v = await Guest.PingAsync().ConfigureAwait(false);
            return v.ValueKind == JsonValueKind.Object
                && v.TryGetProperty("pong", out var pong)
                && pong.ValueKind == JsonValueKind.True;
        }

        return await Zbrt.Run(c => c.HealthAsync()).ConfigureAwait(false);
    }

    /// <summary>Run <paramref name="args"/> in the guest; stdin is ZBRT-only
    /// (NDJSON fails closed). <c>cwd</c> defaults to the workspace root.</summary>
    /// <param name="args">Non-empty argv to execute.</param>
    /// <param name="cwd">Working directory inside the guest (default /workspace).</param>
    /// <param name="timeoutS">Deadline for the command in seconds (default 60).</param>
    /// <param name="stdin">Standard input; only carried by the ZBRT transport.</param>
    /// <returns>The aggregated result of the single exec turn.</returns>
    /// <exception cref="ValidationException">Empty argv, bad cwd/timeout, non-empty
    /// stdin over NDJSON, or (ZBRT) argc &gt; 255 / stdin &gt; 16 MiB — all before any frame.</exception>
    /// <exception cref="TransportException">Connection failure or timeout.</exception>
    /// <exception cref="RemoteException">The guest reported an error or exceeded the output cap.</exception>
    /// <exception cref="DecodeException">The response could not be decoded.</exception>
    public async Task<ExecResult> Exec(IReadOnlyList<string> args, string cwd = "/workspace", double timeoutS = 60.0, byte[]? stdin = null)
    {
        if (args is null)
        {
            throw new ValidationException("argv is required");
        }
        if (args.Count == 0)
        {
            throw new ValidationException("argv is empty");
        }

        GuestValidation.FilePath(cwd);
        GuestValidation.Timeout(timeoutS);

        if (Transport == "ndjson")
        {
            // The NDJSON exec wire contract has no stdin channel: non-empty
            // stdin would run the command WITHOUT its input, so fail closed
            // (mirrors the Rust baseline).
            if (stdin is { Length: > 0 })
            {
                throw new ValidationException("stdin is only supported over the ZBRT transport");
            }
            var v = await Guest.ExecAsync(
                cwd, args, ForkdGuestNdjson.TimeoutSecs(timeoutS), ExecReadBudget(timeoutS)).ConfigureAwait(false);
            return GuestResults.ParseExec(v);
        }

        // ZBRT argc fits one header byte and payloads are u32-bounded: reject
        // locally, with zero frames and no TCP connection (UNIFIED_API.md §4).
        GuestValidation.ZbrtArgc(args.Count);
        GuestValidation.PayloadSize((stdin ?? []).Length, GuestValidation.MaxZbrtPayloadBytes);
        var outcome = await Zbrt.Run(c => c.ExecuteAsync(args, cwd, stdin ?? [], TimeoutMs(timeoutS))).ConfigureAwait(false);
        // ZBRT v1 has no timed-out wire flag: the guest's executor surfaces a
        // deadline miss as an Error frame (an exception here), so the flag is
        // structurally false on this transport (mirrors the Rust baseline).
        return new ExecResult(outcome.ExitCode, outcome.Stdout, outcome.Stderr, false);
    }

    /// <summary>Evaluate a code snippet in the guest; output maps to stdout.
    /// Not supported over the ZBRT transport (fails closed locally).</summary>
    /// <param name="code">Code snippet; blank or &gt; 1 MiB is rejected.</param>
    /// <param name="cwd">Optional guest working directory.</param>
    /// <param name="timeoutS">Optional deadline in seconds (&gt; 0).</param>
    /// <returns>The eval result: output in <see cref="ExecResult.Stdout"/>, stderr always empty.</returns>
    /// <exception cref="ValidationException">Invalid code/cwd/timeout, or ZBRT transport.</exception>
    /// <exception cref="TransportException">Connection failure or timeout.</exception>
    /// <exception cref="RemoteException">The guest reported an error.</exception>
    /// <exception cref="DecodeException">The response could not be decoded.</exception>
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
            var v = await Guest.EvalAsync(
                code, cwd, timeoutS,
                timeoutS.HasValue ? ExecReadBudget(timeoutS.Value) : null).ConfigureAwait(false);
            return GuestResults.ParseEval(v);
        }

        // ZBRT v1 has no eval opcode and the reference guest maps Execute
        // verbatim onto `exec` — fail closed instead of running a literal
        // `eval <code>` command (mirrors the Rust facade).
        throw new ValidationException("eval is not supported over the ZBRT transport");
    }

    /// <summary>Directory entries under <paramref name="path"/> (default ".").</summary>
    /// <param name="path">Guest fs path (relative or /workspace-prefixed).</param>
    /// <returns>The entries under the path (capped at 1000).</returns>
    /// <exception cref="ValidationException">Invalid path.</exception>
    /// <exception cref="DecodeException">The result shape was invalid.</exception>
    public async Task<IReadOnlyList<DirEntry>> Ls(string path = ".")
    {
        GuestValidation.FsPath(path);
        if (Transport == "ndjson")
        {
            var v = await Guest.ToolAsync("ls", new Dictionary<string, object?>
            {
                ["path"] = path,
                ["max_results"] = GuestValidation.MaxResults,
            }).ConfigureAwait(false);
            return GuestResults.ParseLs(v);
        }

        var result = await Zbrt.Run(c => c.FsAsync(FsOp.Ls, path, LsJson)).ConfigureAwait(false);
        return GuestResults.ParseLs(result);
    }

    /// <summary>Find guest paths whose file name matches <paramref name="pattern"/> (path first, UNIFIED_API.md §4).</summary>
    /// <param name="path">Guest fs path to walk.</param>
    /// <param name="pattern">Glob name pattern (≤ 1024 bytes, non-empty).</param>
    /// <returns>Matching paths, relative to <paramref name="path"/> (capped at 1000).</returns>
    /// <exception cref="ValidationException">Invalid path or pattern.</exception>
    /// <exception cref="DecodeException">The result shape was invalid.</exception>
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
    /// <param name="pattern">Glob name pattern.</param>
    /// <returns>Matching paths under the working directory.</returns>
    public Task<IReadOnlyList<string>> Find(string pattern) => Find(".", pattern);

    private async Task<IReadOnlyList<string>> FindCore(string path, string pattern)
    {
        var v = await Guest.ToolAsync("find", new Dictionary<string, object?>
        {
            ["path"] = path,
            ["pattern"] = pattern,
            ["max_results"] = GuestValidation.MaxResults,
        }).ConfigureAwait(false);
        return GuestResults.ParseFind(v);
    }

    private async Task<IReadOnlyList<string>> FindZbrt(string path, string pattern)
    {
        var json = JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["pattern"] = pattern,
            ["max_results"] = GuestValidation.MaxResults,
        });
        var result = await Zbrt.Run(c => c.FsAsync(FsOp.Find, path, json)).ConfigureAwait(false);
        return GuestResults.ParseFind(result);
    }

    /// <summary>Grep guest file contents (path first, UNIFIED_API.md §4).</summary>
    /// <param name="path">Guest fs path to search.</param>
    /// <param name="pattern">Non-empty pattern (≤ 1024 bytes).</param>
    /// <returns>Matching lines with optional location (capped at 1000 matches / 50 KiB).</returns>
    /// <exception cref="ValidationException">Invalid path or pattern.</exception>
    /// <exception cref="DecodeException">The result shape was invalid.</exception>
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
    /// <param name="pattern">Non-empty pattern.</param>
    /// <returns>Matching lines under the working directory.</returns>
    public Task<IReadOnlyList<GrepMatch>> Grep(string pattern) => Grep(".", pattern);

    private async Task<IReadOnlyList<GrepMatch>> GrepCore(string path, string pattern)
    {
        var v = await Guest.ToolAsync("grep", new Dictionary<string, object?>
        {
            ["path"] = path,
            ["pattern"] = pattern,
            ["max_results"] = GuestValidation.MaxResults,
            ["max_bytes"] = GuestValidation.MaxResultBytes,
        }).ConfigureAwait(false);
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
        var result = await Zbrt.Run(c => c.FsAsync(FsOp.Grep, path, json)).ConfigureAwait(false);
        return GuestResults.ParseGrep(result);
    }

    /// <summary>Read a guest file with optional offset / maxBytes cap.</summary>
    /// <param name="path">Guest file path (relative or absolute, non-escaping).</param>
    /// <param name="offset">Optional byte offset to start reading at.</param>
    /// <param name="maxBytes">Optional cap; must be 1..=51200 when present.</param>
    /// <returns>The file bytes with truncation metadata.</returns>
    /// <exception cref="ValidationException">Invalid path or out-of-range maxBytes.</exception>
    /// <exception cref="DecodeException">The result shape was invalid.</exception>
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

            var v = await Guest.ToolAsync("read", args).ConfigureAwait(false);
            return GuestResults.ParseRead(v);
        }

        var json = JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["offset"] = offset,
            ["max_bytes"] = maxBytes,
        });
        var result = await Zbrt.Run(c => c.FsAsync(FsOp.Read, path, json)).ConfigureAwait(false);
        return GuestResults.ParseRead(result);
    }

    /// <summary>Write (or append) <paramref name="data"/>; returns bytes written.</summary>
    /// <param name="path">Guest file path (relative or absolute, non-escaping).</param>
    /// <param name="data">Payload bytes (≤ 51200).</param>
    /// <param name="append">Append instead of truncate-create.</param>
    /// <param name="mode">Optional file mode (e.g. 0o644).</param>
    /// <returns>Number of bytes written as confirmed by the guest.</returns>
    /// <exception cref="ValidationException">Invalid path or oversized payload.</exception>
    /// <exception cref="DecodeException">The result lacked bytes_written.</exception>
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

            var v = await Guest.ToolAsync("write", args).ConfigureAwait(false);
            return GuestResults.ParseWrite(v);
        }

        var json = JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["data"] = WireJson.ByteList(data),
            ["append"] = append,
            ["mode"] = mode,
        });
        var result = await Zbrt.Run(c => c.FsAsync(FsOp.Write, path, json)).ConfigureAwait(false);
        return GuestResults.ParseWrite(result);
    }

    /// <summary>Start an interactive stream. env/pty are NDJSON-only options.</summary>
    /// <param name="args">Non-empty argv to run under the stream.</param>
    /// <param name="cwd">Optional guest working directory.</param>
    /// <param name="pty">Allocate a pseudo-terminal (NDJSON only).</param>
    /// <param name="env">Extra environment variables (NDJSON only).</param>
    /// <returns>An interactive stream handle.</returns>
    /// <exception cref="ValidationException">Empty argv, invalid cwd, or (ZBRT)
    /// pty/env/argc — all rejected before any frame is sent.</exception>
    /// <exception cref="TransportException">Connection failure or timeout.</exception>
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
            var session = await Guest.StreamAsync(args, cwd, pty, envObj).ConfigureAwait(false);
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

        // ZBRT argc fits one header byte: reject locally, with zero frames and
        // no TCP connection (PROTOCOL.md §3.2).
        GuestValidation.ZbrtArgc(args.Count);

        var zbrtSession = await Zbrt.BorrowStreamAsync(args, cwd).ConfigureAwait(false);
        return new GuestStream(zbrtSession);
    }

    /// <summary>Delete this sandbox (2xx/404 both succeed).</summary>
    /// <exception cref="HttpStatusException">The controller returned a non-2xx, non-404 status.</exception>
    public async Task Delete()
    {
        if (Transport == "ndjson")
        {
            _ndjson.Value.DrainPool();
        }
        else
        {
            _zbrt.Value.Drain();
        }
        await _client.DeleteSandbox(Id).ConfigureAwait(false);
    }

    /// <summary>Async disposal — deletes the sandbox (2xx/404 both succeed);
    /// <c>await using</c> guarantees cleanup on a mid-flow failure.</summary>
    public async ValueTask DisposeAsync()
    {
        await Delete().ConfigureAwait(false);
    }

    // Constant ZBRT fs-request bodies (payload never varies → build once).
    private static readonly byte[] LsJson =
        JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["max_results"] = GuestValidation.MaxResults,
        });

    /// <summary>Fixed margin on top of the exec read budget (Python <c>_guest.py</c> baseline).</summary>
    private static readonly TimeSpan ExecReadMargin = TimeSpan.FromSeconds(5);

    /// <summary>
    /// NDJSON exec/eval read budget: client timeout + exec deadline + 5 s
    /// (PROTOCOL.md §2.1, mirroring the Java <c>execReadBudget</c>) so the
    /// guest's own timeout error surfaces instead of the client's.
    /// </summary>
    private TimeSpan ExecReadBudget(double execTimeoutS) =>
        _timeout + TimeSpan.FromMilliseconds(Math.Ceiling(execTimeoutS * 1000.0)) + ExecReadMargin;

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
