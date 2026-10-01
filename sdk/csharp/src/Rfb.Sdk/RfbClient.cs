using System.Text.Json;
using Rfb.Sdk.Internal;

namespace Rfb.Sdk;

/// <summary>
/// The single public client type of the unified RFB SDK (UNIFIED_API.md §2-§3).
/// The Rust implementation <c>rfb::client::RfbClient</c> is the reference; this
/// class mirrors it method by method.
/// </summary>
public sealed class RfbClient : IDisposable
{
    private readonly ForkdControllerHttp _controller;
    private readonly TimeSpan _timeout;

    /// <summary>Guest transport: NDJSON over the agent's TCP port.</summary>
    public const string TransportNdjson = "ndjson";
    /// <summary>Guest transport: ZBRT v1 frames.</summary>
    public const string TransportZbrt = "zbrt";

    /// <summary>Default ZBRT bridge TCP endpoint (RFB_ZBRT_TCP default).</summary>
    public const string DefaultZbrtTcp = "127.0.0.1:15000";

    /// <summary>Client timeout in seconds (mirrors the other SDKs).</summary>
    public double TimeoutS => _timeout.TotalSeconds;

    /// <summary>
    /// Create a client. <paramref name="baseUrl"/> defaults to env FORKD_URL
    /// (http://127.0.0.1:8889 — unset or blank both fall back); <paramref
    /// name="token"/> defaults to env FORKD_TOKEN (a Bearer header is sent
    /// only when non-empty).
    /// </summary>
    /// <param name="baseUrl">forkd controller base URL (http/https with host).</param>
    /// <param name="token">Controller Bearer token; null/empty sends no header.</param>
    /// <param name="timeoutS">Per-request timeout (connect + read) in seconds; must be &gt; 0 and finite.</param>
    /// <exception cref="ValidationException">Invalid base URL or non-positive/non-finite timeout.</exception>
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

        if (double.IsNaN(timeoutS) || double.IsInfinity(timeoutS) || timeoutS <= 0)
        {
            throw new ValidationException("timeout must be a positive, finite number of seconds");
        }
        // Clamp before converting: HttpClient.Timeout rejects spans above
        // ~24.8 days with a non-RfbException, so a unit mix-up (ms as s) must
        // surface as validation, not as a raw setter throw.
        _timeout = TimeSpan.FromSeconds(Math.Min(timeoutS, 86_400));
        _controller = new ForkdControllerHttp(url, tok, _timeout);
    }

    /// <summary>All snapshots as reported by the controller (<c>GET /v1/snapshots</c>).</summary>
    /// <returns>The snapshots known to the controller.</returns>
    /// <exception cref="HttpStatusException">The controller returned a non-2xx status.</exception>
    /// <exception cref="TransportException">Connection failure or timeout.</exception>
    /// <exception cref="DecodeException">The controller response could not be decoded.</exception>
    public async Task<IReadOnlyList<Snapshot>> ListSnapshots()
    {
        var arr = await _controller.ListSnapshotsAsync().ConfigureAwait(false);
        return ParseList<Snapshot>(arr);
    }

    /// <summary>Snapshot detail via /info → legacy fallback; both 404 → null.</summary>
    /// <param name="tag">Snapshot tag to look up.</param>
    /// <returns>The snapshot, or null when the controller does not know it.</returns>
    /// <exception cref="HttpStatusException">The controller returned a non-2xx (non-404) status.</exception>
    /// <exception cref="TransportException">Connection failure or timeout.</exception>
    /// <exception cref="DecodeException">The controller response could not be decoded.</exception>
    public async Task<Snapshot?> Snapshot(string tag)
    {
        var v = await _controller.SnapshotInfoAsync(tag).ConfigureAwait(false);
        return v is null ? null : ParseOne<Snapshot>(v.Value);
    }

    /// <summary>Poll every 100 ms until ready; "failed" → RemoteException; timeout → TransportException.</summary>
    /// <param name="tag">Snapshot tag to wait for.</param>
    /// <param name="timeoutS">Wait budget in seconds (default 60); a timeout is a transport-class error.</param>
    /// <returns>The ready and bootable snapshot.</returns>
    /// <exception cref="RemoteException">The snapshot reported a failed status.</exception>
    /// <exception cref="TransportException">The snapshot did not become ready within the budget.</exception>
    /// <exception cref="ValidationException">Non-positive or non-finite <paramref name="timeoutS"/>.</exception>
    public async Task<Snapshot> WaitSnapshot(string tag, double timeoutS = 60)
    {
        // Fail closed like the Java/Python baselines: NaN/Infinity would
        // otherwise leak ArgumentException/OverflowException instead of an
        // RfbException subtype.
        GuestValidation.Timeout(timeoutS);
        var deadline =
            DateTime.UtcNow + TimeSpan.FromSeconds(Math.Min(timeoutS, 86_400));
        while (true)
        {
            var snapshots = await ListSnapshots().ConfigureAwait(false);
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

            await Task.Delay(100).ConfigureAwait(false);
        }
    }

    /// <summary>Create `n` sandboxes from a bootable snapshot tag.</summary>
    /// <param name="snapshotTag">Bootable snapshot tag to spawn from.</param>
    /// <param name="n">Number of sandboxes to create.</param>
    /// <param name="perChildNetns">Give each child its own network namespace.</param>
    /// <param name="memoryLimitMib">Memory cap in MiB (null = controller default).</param>
    /// <param name="prewarm">Ask the controller to prewarm the sandbox.</param>
    /// <param name="liveFork">Fork the sandbox live from the parent VM.</param>
    /// <param name="hugepages">Use hugepages for the guest memory.</param>
    /// <param name="transport">Guest transport of the returned handles: "ndjson" (default) or "zbrt".</param>
    /// <returns>The created sandboxes.</returns>
    /// <exception cref="ValidationException">Invalid transport (rejected before any request).</exception>
    /// <exception cref="HttpStatusException">The controller returned a non-2xx status.</exception>
    /// <exception cref="TransportException">Connection failure or timeout.</exception>
    /// <exception cref="DecodeException">The controller response could not be decoded.</exception>
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
        // Fail closed on the transport BEFORE any network traffic (§9.8):
        // validating only in the Sandbox ctor would leak an invalid value
        // into an already-issued POST.
        GuestValidation.Transport(transport);
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
        var arr = await _controller.CreateSandboxesAsync(body).ConfigureAwait(false);
        return ToSandboxes(ParseList<SandboxInfo>(arr), transport);
    }

    /// <summary>Live sandboxes; `transport` selects the guest transport of the returned handles.</summary>
    /// <param name="transport">Guest transport of the returned handles: "ndjson" (default) or "zbrt".</param>
    /// <returns>The live sandboxes as reported by the controller.</returns>
    /// <exception cref="ValidationException">Invalid transport (rejected before any request).</exception>
    public async Task<IReadOnlyList<Sandbox>> ListSandboxes(string transport = "ndjson")
    {
        GuestValidation.Transport(transport);
        var arr = await _controller.ListSandboxesAsync().ConfigureAwait(false);
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
    /// <param name="sandboxOrId">A <see cref="Sandbox"/> handle, or a sandbox id string.</param>
    /// <param name="transport">
    /// Guest transport override: only an EXPLICIT value overrides an attached
    /// <see cref="Sandbox"/> handle (null keeps the handle's transport); an id
    /// string resolves via the live pool with null meaning "ndjson".
    /// </param>
    /// <returns>The attached sandbox handle.</returns>
    /// <exception cref="ValidationException">Invalid transport, invalid id, or unsupported argument type.</exception>
    /// <exception cref="RemoteException">No live sandbox matches the id ("sandbox not found").</exception>
    public async Task<Sandbox> Connect(object sandboxOrId, string? transport = null)
    {
        GuestValidation.Transport(transport);
        switch (sandboxOrId)
        {
            case Sandbox existing:
                // Rust attach semantics (ConnectTarget for &Sandbox): attach the
                // handle as-is; only an explicitly passed transport overrides it.
                return transport is null
                    ? existing
                    : new Sandbox(this, existing.Info, transport, _timeout);
            case string id:
                {
                    GuestValidation.Id(id);
                    var resolved = transport ?? TransportNdjson;
                    var list = await ListSandboxes(resolved).ConfigureAwait(false);
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
    /// <param name="id">Sandbox id to ping.</param>
    /// <returns>The controller's ping response JSON.</returns>
    /// <exception cref="ValidationException">Invalid sandbox id.</exception>
    /// <exception cref="HttpStatusException">The controller returned a non-2xx status.</exception>
    public async Task<JsonElement> PingSandbox(string id) =>
        await _controller.PingAsync(id).ConfigureAwait(false);

    /// <summary>Delete a sandbox; 2xx and 404 are both success.</summary>
    /// <param name="id">Sandbox id to delete.</param>
    /// <exception cref="ValidationException">Invalid sandbox id.</exception>
    /// <exception cref="HttpStatusException">The controller returned a non-2xx, non-404 status.</exception>
    public async Task DeleteSandbox(string id) =>
        await _controller.DeleteSandboxAsync(id).ConfigureAwait(false);

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

    /// <summary>Dispose the underlying controller HTTP client.</summary>
    public void Dispose() => _controller.Dispose();
}
