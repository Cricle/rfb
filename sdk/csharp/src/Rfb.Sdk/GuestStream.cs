using System.Text.Json;
using Rfb.Sdk.Internal;

namespace Rfb.Sdk;

/// <summary>
/// Interactive guest stream (UNIFIED_API.md §5). Events: started | stdout |
/// stderr | exit. Clean close → NextEvent returns null.
/// </summary>
public sealed class GuestStream : IDisposable
{
    private readonly ForkdGuestNdjsonStream? _ndjson;
    private readonly ZbrtStreamSession? _zbrt;

    internal GuestStream(ForkdGuestNdjsonStream session) => _ndjson = session;

    internal GuestStream(ZbrtStreamSession session) => _zbrt = session;

    /// <summary>Release the underlying transport sockets (idempotent).</summary>
    public void Dispose()
    {
        _ndjson?.Dispose();
        _zbrt?.Dispose();
    }

    /// <summary>Next stream event; null when the stream closed cleanly.</summary>
    public async Task<StreamEvent?> NextEvent()
    {
        if (_ndjson is not null)
        {
            var value = await _ndjson.NextEventAsync().ConfigureAwait(false);
            return value is null ? null : GuestResults.MapStreamEvent(value.Value);
        }

        return await _zbrt!.NextEventAsync().ConfigureAwait(false);
    }

    /// <summary>Send stdin text; calling after the terminal event raises RemoteException.</summary>
    public async Task SendInput(string text)
    {
        if (_ndjson is not null)
        {
            await _ndjson.SendInputAsync(text).ConfigureAwait(false);
        }
        else
        {
            await _zbrt!.SendInputAsync(text).ConfigureAwait(false);
        }
    }

    /// <summary>Request termination; idempotent.</summary>
    public async Task Stop()
    {
        if (_ndjson is not null)
        {
            await _ndjson.StopAsync().ConfigureAwait(false);
        }
        else
        {
            await _zbrt!.StopAsync().ConfigureAwait(false);
        }
    }
}
