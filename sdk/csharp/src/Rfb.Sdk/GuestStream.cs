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
    private bool _zbrtStartedSent;

    internal GuestStream(ForkdGuestNdjsonStream session) => _ndjson = session;

    internal GuestStream(ZbrtStreamSession session) => _zbrt = session;

    /// <summary>Release the underlying transport sockets (idempotent).</summary>
    public void Dispose()
    {
        _ndjson?.Dispose();
        _zbrt?.Dispose();
    }

    /// <summary>
    /// Next stream event; null once the stream closed cleanly (after the
    /// terminal Exit event or when the peer disconnected). ZBRT has no
    /// started frame: the first call synthesizes one (UNIFIED_API.md §5).
    /// </summary>
    /// <returns>The next event, or null after a clean close.</returns>
    /// <exception cref="TransportException">Read/write failure or timeout.</exception>
    /// <exception cref="DecodeException">A frame or line could not be decoded.</exception>
    /// <exception cref="RemoteException">The guest reported an error.</exception>
    public async Task<StreamEvent?> NextEvent()
    {
        if (_ndjson is not null)
        {
            // Unknown non-terminal lines are ignored (PROTOCOL.md §2.5), so
            // keep reading until a line maps to an event or the stream ends.
            while (true)
            {
                var value = await _ndjson.NextEventAsync().ConfigureAwait(false);
                if (value is null)
                {
                    return null;
                }

                if (GuestResults.MapStreamEvent(value.Value) is { } mapped)
                {
                    return mapped;
                }
            }
        }

        if (!_zbrtStartedSent)
        {
            // ZBRT has no started frame: the first next_event synthesizes one
            // (Rust baseline client/zbrt.rs ZbrtStream::next_event).
            _zbrtStartedSent = true;
            return new StreamEvent(StreamEventKind.Started, [], null);
        }

        return await _zbrt!.NextEventAsync().ConfigureAwait(false);
    }

    /// <summary>
    /// Send stdin text to the running command. ZBRT v1 has no stdin channel:
    /// calling it there raises RemoteException, as does calling after the
    /// terminal event.
    /// </summary>
    /// <param name="text">Input forwarded to the guest process.</param>
    /// <exception cref="RemoteException">Stream already ended, or ZBRT transport.</exception>
    /// <exception cref="TransportException">Write failure or timeout.</exception>
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
    /// <exception cref="TransportException">Write/read failure or timeout.</exception>
    /// <exception cref="RemoteException">The guest reported an error.</exception>
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
