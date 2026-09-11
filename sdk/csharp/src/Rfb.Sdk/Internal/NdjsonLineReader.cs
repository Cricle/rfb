using System.Net.Sockets;
using System.Text;

namespace Rfb.Sdk.Internal;

/// <summary>
/// Stateful NDJSON line reader over a NetworkStream. Retains unconsumed
/// bytes across calls so multiple lines delivered in one TCP segment are
/// never lost (Linux loopback coalesces server writes into one segment; a
/// stateless reader would drop everything after the first line).
/// </summary>
internal sealed class NdjsonLineReader
{
    private readonly NetworkStream _stream;
    private readonly TimeSpan _timeout;
    private readonly byte[] _chunk = new byte[8192];
    private byte[] _pending = new byte[8192]; // staged bytes not yet scanned
    private readonly MemoryStream _line = new(1024);
    private int _start; // start of unscanned bytes in _pending
    private int _end;   // end of unscanned bytes in _pending

    public NdjsonLineReader(NetworkStream stream, TimeSpan timeout)
    {
        _stream = stream;
        _timeout = timeout;
    }

    /// <summary>
    /// Read one \n-terminated line (chunk-scanned, not byte-by-byte). Trailing
    /// \r/\n trimmed. Clean EOF with no partial data → null. With skipEmpty,
    /// empty keepalive lines are skipped (stream sessions). The whole line
    /// must arrive within the timeout.
    /// </summary>
    public async Task<string?> ReadLineAsync(bool skipEmpty)
    {
        using var cts = new CancellationTokenSource(_timeout);
        try
        {
            while (true)
            {
                // 1) Scan bytes staged by previous reads (may hold several lines).
                while (_start < _end)
                {
                    var nl = Array.IndexOf(_pending, (byte)'\n', _start, _end - _start);
                    if (nl < 0)
                    {
                        break;
                    }

                    _line.Write(_pending, _start, nl + 1 - _start);
                    _start = nl + 1;
                    if (_line.Length > ForkdGuestNdjson.MaxLineBytes)
                    {
                        // An oversized line is a decode failure, not a transport
                        // failure (Python/Java classify it the same).
                        throw new DecodeException($"guest response exceeded {ForkdGuestNdjson.MaxLineBytes} bytes");
                    }

                    var line = Trim(_line);
                    _line.SetLength(0);
                    if (line.Length > 0 || !skipEmpty)
                    {
                        return line;
                    }
                }

                // 2) Pending drained: fold the tail fragment (no '\n' yet) into
                //    the line accumulator before reading more from the wire.
                if (_start < _end)
                {
                    _line.Write(_pending, _start, _end - _start);
                    if (_line.Length > ForkdGuestNdjson.MaxLineBytes)
                    {
                        // An oversized line is a decode failure, not a transport
                        // failure (Python/Java classify it the same).
                        throw new DecodeException($"guest response exceeded {ForkdGuestNdjson.MaxLineBytes} bytes");
                    }
                }

                _start = 0;
                _end = 0;

                var n = await _stream.ReadAsync(_chunk, 0, _chunk.Length, cts.Token)
                    .ConfigureAwait(false);
                if (n == 0)
                {
                    if (_line.Length > 0)
                    {
                        throw new DecodeException("guest connection closed mid-line");
                    }

                    return null; // clean EOF
                }

                if (n > _pending.Length)
                {
                    Array.Resize(ref _pending, n);
                }

                Buffer.BlockCopy(_chunk, 0, _pending, 0, n);
                _end = n;
            }
        }
        catch (OperationCanceledException)
        {
            throw new TransportException("guest response timeout");
        }
    }

    private static string Trim(MemoryStream buffer)
    {
        var buf = buffer.GetBuffer();
        var end = (int)buffer.Length;
        while (end > 0 && buf[end - 1] is (byte)'\n' or (byte)'\r')
        {
            end--;
        }

        return Encoding.UTF8.GetString(buf, 0, end);
    }
}
