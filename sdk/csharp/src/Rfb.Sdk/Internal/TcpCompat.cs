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
            try
            {
                await tcp.ConnectAsync(host, port).ConfigureAwait(false);
            }
            catch (ObjectDisposedException)
            {
                // token 触发的 Close 让挂起的 connect 以 ODE 爆出：翻译回
                // 取消语义，让上层把 tcp.Dispose 收尾并报 connect timeout。
                token.ThrowIfCancellationRequested();
                throw;
            }
        }
        token.ThrowIfCancellationRequested();
#endif
    }
}
