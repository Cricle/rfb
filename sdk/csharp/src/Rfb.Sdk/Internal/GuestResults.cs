using System.Text.Json;

namespace Rfb.Sdk.Internal;

/// <summary>
/// Shared result-shape parsing for both guest transports (NDJSON JSON and ZBRT
/// FsResult JSON use identical payload shapes — PROTOCOL.md §2.4).
/// </summary>
internal static class GuestResults
{
    public static ExecResult ParseExec(JsonElement v)
    {
        // Rust baseline: a response without exit_code reads as failure (-1), not success.
        var exit = WireJson.IntOr(v, "exit_code", -1);
        var stdout = WireJson.ValueBytes(Prop(v, "out") ?? Prop(v, "stdout"));
        var stderr = WireJson.ValueBytes(Prop(v, "err") ?? Prop(v, "stderr"));
        var timedOut = WireJson.BoolOr(v, "timed_out", false);
        return new ExecResult(exit, stdout, stderr, timedOut);
    }

    /// <summary>eval output maps to stdout (UNIFIED_API.md §4).</summary>
    public static ExecResult ParseEval(JsonElement v)
    {
        var stdout = WireJson.ValueBytes(Prop(v, "out") ?? Prop(v, "output"));
        var exit = WireJson.IntOr(v, "exit_code", WireJson.IntOr(v, "status", 0));
        var timedOut = WireJson.BoolOr(v, "timed_out", false);
        return new ExecResult(exit, stdout, [], timedOut);
    }

    public static List<DirEntry> ParseLs(JsonElement v)
    {
        var entries = new List<DirEntry>();
        if (!WireJson.HasKey(v, "entries"))
        {
            throw new DecodeException("invalid ls result: missing entries");
        }

        foreach (var item in v.GetProperty("entries").EnumerateArray())
        {
            var name = item.TryGetProperty("name", out var n) ? n.GetString() ?? "" : "";
            var isDir = WireJson.BoolOr(item, "is_dir", false);
            entries.Add(new DirEntry(name, isDir, WireJson.OptInt(item, "size")));
        }

        return entries;
    }

    public static List<string> ParseFind(JsonElement v)
    {
        var matches = new List<string>();
        if (!WireJson.HasKey(v, "matches"))
        {
            throw new DecodeException("invalid find result: missing matches");
        }

        foreach (var item in v.GetProperty("matches").EnumerateArray())
        {
            if (item.ValueKind != JsonValueKind.String)
            {
                throw new DecodeException("invalid find result: matches must be strings");
            }

            matches.Add(item.GetString() ?? "");
        }

        return matches;
    }

    public static List<GrepMatch> ParseGrep(JsonElement v)
    {
        var matches = new List<GrepMatch>();
        if (!WireJson.HasKey(v, "matches"))
        {
            throw new DecodeException("invalid grep result: missing matches");
        }

        foreach (var item in v.GetProperty("matches").EnumerateArray())
        {
            var path = item.TryGetProperty("path", out var p) ? p.GetString() ?? "" : "";
            var text = item.TryGetProperty("text", out var t) ? t.GetString() ?? "" : "";
            matches.Add(new GrepMatch(path, WireJson.OptInt(item, "line"), WireJson.OptInt(item, "column"), text));
        }

        return matches;
    }

    public static FileRead ParseRead(JsonElement v)
    {
        if (!WireJson.HasKey(v, "data"))
        {
            throw new DecodeException("invalid read result: missing data");
        }

        var data = WireJson.ValueBytes(v.GetProperty("data"));
        return new FileRead(data, WireJson.BoolOr(v, "truncated", false), WireJson.OptInt(v, "total_bytes"));
    }

    public static long ParseWrite(JsonElement v) =>
        WireJson.OptInt(v, "bytes_written") ?? throw new DecodeException("invalid write result: missing bytes_written");

    private static readonly (string Key, bool Stderr)[] StreamOutputKeys =
    {
        ("stdout", false), ("out", false), ("stderr", true), ("err", true),
    };

    /// <summary>NDJSON stream event mapping (mirror of forkd_stream_event).</summary>
    public static StreamEvent MapStreamEvent(JsonElement value)
    {
        if (WireJson.BoolOr(value, "started", false)
            || (value.TryGetProperty("stream", out var s) && s.ValueKind == JsonValueKind.String && s.GetString() == "started")
            || (value.TryGetProperty("event", out var e) && e.ValueKind == JsonValueKind.String && e.GetString() == "started"))
        {
            return new StreamEvent(StreamEventKind.Started, [], null);
        }

        if (WireJson.OptInt(value, "exit_code") is { } code)
        {
            return new StreamEvent(StreamEventKind.Exit, [], (int)code);
        }

        if (WireJson.BoolOr(value, "done", false))
        {
            return new StreamEvent(StreamEventKind.Exit, [], null);
        }

        foreach (var (key, stderr) in StreamOutputKeys)
        {
            if (WireJson.HasKey(value, key))
            {
                var data = WireJson.ValueBytes(value.GetProperty(key));
                return new StreamEvent(stderr ? StreamEventKind.Stderr : StreamEventKind.Stdout, data, null);
            }
        }

        throw new DecodeException("invalid guest stream event");
    }

    private static JsonElement? Prop(JsonElement v, string key)
    {
        if (v.ValueKind != JsonValueKind.Object || !v.TryGetProperty(key, out var p))
        {
            return null;
        }

        return p;
    }
}
