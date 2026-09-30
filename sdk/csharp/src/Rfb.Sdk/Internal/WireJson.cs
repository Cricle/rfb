using System.Text;
using System.Text.Json;

namespace Rfb.Sdk.Internal;

/// <summary>Shared JSON wire helpers for both guest transports.</summary>
internal static class WireJson
{
    /// <summary>Keys whose presence marks a terminal NDJSON response line (PROTOCOL.md §2.1).</summary>
    public static readonly string[] TerminalKeys = {
        "exit_code", "pong", "results", "entries", "matches", "data", "content",
        "output", "status", "ok", "healthy", "done", "cancelled", "bytes_written",
    };

    public static bool IsObject(JsonElement value) => value.ValueKind == JsonValueKind.Object;

    public static bool HasKey(JsonElement value, string key) =>
        value.ValueKind == JsonValueKind.Object && value.TryGetProperty(key, out _);

    public static bool IsTerminalLine(JsonElement value)
    {
        if (value.ValueKind != JsonValueKind.Object)
        {
            return false;
        }

        foreach (var key in TerminalKeys)
        {
            if (value.TryGetProperty(key, out _))
            {
                return true;
            }
        }

        return false;
    }

    /// <summary>Any response line containing a string "error" key is a remote error (fail closed).</summary>
    public static void CheckRemoteError(JsonElement value)
    {
        if (value.ValueKind == JsonValueKind.Object
            && value.TryGetProperty("error", out var error)
            && error.ValueKind == JsonValueKind.String)
        {
            throw new RemoteException(error.GetString() ?? "guest returned error");
        }
    }

    /// <summary>Parse "host:port" guest address; throws DecodeException when malformed.</summary>
    public static (string Host, int Port) ParseGuestAddress(string address)
    {
        var sep = address.LastIndexOf(':');
        if (sep <= 0 || sep == address.Length - 1
            || !int.TryParse(address[(sep + 1)..], out var port) || port is <= 0 or > 65535)
        {
            throw new DecodeException("invalid forkd guest address");
        }

        return (address[..sep], port);
    }

    /// <summary>
    /// Convert a JSON value into bytes: string → UTF-8 bytes, number array → bytes,
    /// null/missing → empty (mirror of forkd_value_bytes).
    /// </summary>
    public static byte[] ValueBytes(JsonElement? value)
    {
        if (value is null)
        {
            return [];
        }

        var v = value.Value;
        switch (v.ValueKind)
        {
            case JsonValueKind.String:
                return Encoding.UTF8.GetBytes(v.GetString() ?? "");
            case JsonValueKind.Array:
                {
                    var bytes = new byte[v.GetArrayLength()];
                    var i = 0;
                    foreach (var item in v.EnumerateArray())
                    {
                        if (item.ValueKind != JsonValueKind.Number || !item.TryGetInt64(out var n) || n is < 0 or > 255)
                        {
                            throw new DecodeException("execution output must be UTF-8 or byte array");
                        }

                        bytes[i++] = (byte)n;
                    }

                    return bytes;
                }
            case JsonValueKind.Null:
            case JsonValueKind.Undefined:
                return [];
            default:
                throw new DecodeException("execution output must be UTF-8 or byte array");
        }
    }

    public static long? OptInt(JsonElement value, string key)
    {
        if (value.ValueKind != JsonValueKind.Object || !value.TryGetProperty(key, out var v))
        {
            return null;
        }

        return v.ValueKind switch
        {
            JsonValueKind.Number when v.TryGetInt64(out var n) => n,
            _ => null,
        };
    }

    public static int IntOr(JsonElement value, string key, int fallback) => OptInt(value, key) is { } n ? (int)n : fallback;

    public static bool BoolOr(JsonElement value, string key, bool fallback)
    {
        if (value.ValueKind != JsonValueKind.Object || !value.TryGetProperty(key, out var v))
        {
            return fallback;
        }

        return v.ValueKind switch
        {
            JsonValueKind.True => true,
            JsonValueKind.False => false,
            _ => fallback,
        };
    }

    /// <summary>Byte array as a JSON number array element value (never base64).
    /// List&lt;byte&gt; serializes identically to the old boxed List&lt;object&gt; ([1,2,3]) without per-byte boxing.</summary>
    public static object ByteList(byte[] data)
    {
        var list = new List<byte>(data.Length);
        list.AddRange(data);
        return list;
    }
}
