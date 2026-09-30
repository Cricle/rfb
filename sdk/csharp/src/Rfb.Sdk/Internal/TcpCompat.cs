using System.Net.Sockets;

namespace Rfb.Sdk.Internal;

/// <summary>Cross-TFM TCP helpers (netstandard2.0/2.1 lack the modern overloads).</summary>
internal static class TcpCompat
{
    /// <summary>Connect with cancellation on every target framework.</summary>
    public static async Task ConnectAsync(TcpClient tcp, string host, int port, CancellationToken token)
    {
#if NET8_0_OR_GREATER
        await tcp.ConnectAsync(host, port, token).ConfigureAwait(false);
#else
        // Downlevel: no token overload - close the socket to unblock the
        // pending connect when the token fires.
        using (token.Register(static state => ((TcpClient)state!).Close(), tcp))
        {
            await tcp.ConnectAsync(host, port).ConfigureAwait(false);
        }
        token.ThrowIfCancellationRequested();
#endif
    }
}
