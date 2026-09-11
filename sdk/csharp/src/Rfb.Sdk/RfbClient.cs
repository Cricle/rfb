using System.Text.Json;
using Rfb.Sdk.Internal;

namespace Rfb.Sdk;

/// <summary>
/// The single public client type of the unified RFB SDK (UNIFIED_API.md §2-§3).
/// The Rust implementation <c>rfb::client::RfbClient</c> is the reference; this
/// class mirrors it method by method.
/// </summary>
public sealed class RfbClient
{
    private readonly ForkdControllerHttp _controller;
    private readonly TimeSpan _timeout;

    /// <summary>
    /// <paramref name="baseUrl"/> defaults to env FORKD_URL (http://127.0.0.1:8889);
    /// <paramref name="token"/> defaults to env FORKD_TOKEN (non-empty only).
    /// </summary>
    public RfbClient(string? baseUrl = null, string? token = null, double timeoutS = 10.0)
    {
        var url = baseUrl ?? Environment.GetEnvironmentVariable("FORKD_URL");
        if (string.IsNullOrWhiteSpace(url))
        {
            url = ForkdControllerHttp.DefaultBaseUrl;
        }

        var tok = token ?? Environment.GetEnvironmentVariable("FORKD_TOKEN");
        if (string.IsNullOrWhiteSpace(tok))
        {
            tok = null;
        }

        if (timeoutS <= 0)
        {
            throw new ValidationException("timeout must be positive");
        }

        _timeout = TimeSpan.FromSeconds(timeoutS);
        _controller = new ForkdControllerHttp(url, tok, _timeout);
    }

    public async Task<IReadOnlyList<Snapshot>> ListSnapshots()
    {
        var arr = await _controller.ListSnapshotsAsync();
        return ParseList<Snapshot>(arr);
    }

    /// <summary>Snapshot detail via /info → legacy fallback; both 404 → null.</summary>
    public async Task<Snapshot?> Snapshot(string tag)
    {
        var v = await _controller.SnapshotInfoAsync(tag);
        return v is null ? null : ParseOne<Snapshot>(v.Value);
    }

    /// <summary>Poll every 100 ms until ready; "failed" → RemoteException; timeout → TransportException.</summary>
    public async Task<Snapshot> WaitSnapshot(string tag, double timeoutS = 60)
    {
        var deadline = DateTime.UtcNow + TimeSpan.FromSeconds(timeoutS);
        while (true)
        {
            var snapshots = await ListSnapshots();
            Snapshot? match = null;
            foreach (var s in snapshots)
            {
                if (s.Tag == tag)
                {
                    match = s;
                    break;
                }
            }

            if (match is not null)
            {
                if (string.Equals(match.Status, "failed", StringComparison.OrdinalIgnoreCase))
                {
                    throw new RemoteException($"forkd snapshot `{tag}` is Failed");
                }

                if (string.Equals(match.Status, "ready", StringComparison.OrdinalIgnoreCase) && match.Bootable)
                {
                    return match;
                }
            }

            if (DateTime.UtcNow >= deadline)
            {
                throw new TransportException($"forkd snapshot `{tag}` did not become Ready before timeout");
            }

            await Task.Delay(100);
        }
    }

    public async Task<IReadOnlyList<Sandbox>> CreateSandbox(
        string snapshotTag,
        int n = 1,
        bool perChildNetns = false,
        long? memoryLimitMib = null,
        bool prewarm = false,
        bool liveFork = false,
        bool hugepages = false,
        string transport = "ndjson")
    {
        var body = new Dictionary<string, object?>
        {
            ["snapshot_tag"] = snapshotTag,
            ["n"] = n,
            ["per_child_netns"] = perChildNetns,
            ["memory_limit_mib"] = memoryLimitMib,
            ["prewarm"] = prewarm,
            ["live_fork"] = liveFork,
            ["hugepages"] = hugepages,
        };
        var arr = await _controller.CreateSandboxesAsync(body);
        return ToSandboxes(ParseList<SandboxInfo>(arr), transport);
    }

    public async Task<IReadOnlyList<Sandbox>> ListSandboxes(string transport = "ndjson")
    {
        var arr = await _controller.ListSandboxesAsync();
        return ToSandboxes(ParseList<SandboxInfo>(arr), transport);
    }

    private List<Sandbox> ToSandboxes(List<SandboxInfo> infos, string transport)
    {
        var result = new List<Sandbox>(infos.Count);
        foreach (var info in infos)
        {
            result.Add(new Sandbox(this, info, transport, _timeout));
        }

        return result;
    }

    /// <summary>Attach an existing sandbox by id string or by Sandbox value.</summary>
    public async Task<Sandbox> Connect(object sandboxOrId, string transport = "ndjson")
    {
        switch (sandboxOrId)
        {
            case Sandbox existing:
                // Attach as-is: an existing handle keeps its transport instead
                // of being silently reset to the default.
                return existing;
            case string id:
                {
                    GuestValidation.Id(id);
                    var list = await ListSandboxes(transport);
                    foreach (var sandbox in list)
                    {
                        if (sandbox.Id == id)
                        {
                            return sandbox;
                        }
                    }

                    throw new RemoteException($"sandbox `{id}` not found");
                }
            default:
                throw new ValidationException("connect accepts a Sandbox or a sandbox id string");
        }
    }

    /// <summary>Raw controller ping reply, returned as-is.</summary>
    public async Task<JsonElement> PingSandbox(string id) =>
        await _controller.PingAsync(id);

    /// <summary>Delete a sandbox; 2xx and 404 are both success.</summary>
    public async Task DeleteSandbox(string id) =>
        await _controller.DeleteSandboxAsync(id);

    internal TimeSpan Timeout => _timeout;

    private static List<T> ParseList<T>(JsonElement arr)
        where T : class
    {
        if (arr.ValueKind != JsonValueKind.Array)
        {
            throw new DecodeException("invalid forkd response: expected array");
        }

        var list = new List<T>(arr.GetArrayLength());
        foreach (var item in arr.EnumerateArray())
        {
            list.Add(ParseOne<T>(item));
        }

        return list;
    }

    private static T ParseOne<T>(JsonElement item)
        where T : class
    {
        try
        {
            return item.Deserialize<T>() ?? throw new DecodeException("invalid forkd response: null element");
        }
        catch (JsonException e)
        {
            throw new DecodeException($"invalid forkd response: {e.Message}");
        }
    }
}
