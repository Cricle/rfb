using System.Text;

namespace Rfb.Sdk.Internal;

/// <summary>
/// Guest-side limits and path/pattern/id validators (PROTOCOL.md §2.3; mirror of
/// rfb/src/guest/limits.rs). All checks run locally before anything is sent — fail closed.
/// </summary>
internal static class GuestValidation
{
    public const int MaxPathBytes = 4096;
    public const int MaxPatternBytes = 1024;
    public const int MaxResults = 1000;
    public const int MaxResultBytes = 51200; // 50 KiB
    public const int MaxCodeBytes = 1048576; // 1 MiB

    private static readonly UTF8Encoding StrictUtf8 = new(encoderShouldEmitUTF8Identifier: false, throwOnInvalidBytes: true);

    private static int Utf8Len(string s)
    {
        try
        {
            return StrictUtf8.GetByteCount(s);
        }
        catch (EncoderFallbackException)
        {
            // GetByteCount is the encoding side: an unpaired surrogate raises
            // EncoderFallbackException (not DecoderFallbackException). Treat it
            // as over-limit instead of leaking a non-RfbException.
            return int.MaxValue;
        }
    }

    private static bool ContainsNul(string s) => s.IndexOf('\0') >= 0;

    private static bool HasDotDotSegment(string path)
    {
        foreach (var segment in path.Split('/'))
        {
            if (segment == "..")
            {
                return true;
            }
        }

        return false;
    }

    /// <summary>Structured fs paths (ls/find/grep): guest-relative or /workspace-prefixed only.</summary>
    public static void FsPath(string path)
    {
        if (path.Length == 0
            || Utf8Len(path) > MaxPathBytes
            || ContainsNul(path)
            || (path.StartsWith('/') && !IsWorkspacePath(path))
            || path.IndexOf('\\') >= 0
            || HasDotDotSegment(path))
        {
            throw new ValidationException(
                "invalid guest path: must be a non-empty, relative, non-escaping guest path");
        }
    }

    /// <summary>Absolute fs paths are only allowed under the /workspace root (segment boundary enforced).</summary>
    private static bool IsWorkspacePath(string path) =>
        path.Equals("/workspace", StringComparison.Ordinal)
        || path.StartsWith("/workspace/", StringComparison.Ordinal);

    /// <summary>File paths (read/write and eval/stream cwd): guest-absolute or relative, no host form.</summary>
    public static void FilePath(string path)
    {
        if (path.Length == 0
            || Utf8Len(path) > MaxPathBytes
            || ContainsNul(path)
            || path.Contains('\\')
            || (path.Length >= 2 && path[1] == ':')
            || HasDotDotSegment(path))
        {
            throw new ValidationException(
                "invalid guest path: must be a non-empty, non-escaping guest path");
        }
    }

    public static void Pattern(string pattern)
    {
        if (pattern.Length == 0 || ContainsNul(pattern))
        {
            throw new ValidationException("invalid guest pattern: must be non-empty without NUL");
        }

        if (Utf8Len(pattern) > MaxPatternBytes)
        {
            throw new ValidationException("guest pattern limit exceeded");
        }
    }

    /// <summary>Count/size limits: must be &gt; 0 and ≤ <paramref name="max"/> (0 rejected).</summary>
    public static void Limit(long value, long max)
    {
        if (value <= 0 || value > max)
        {
            throw new ValidationException("guest result limit exceeded");
        }
    }

    /// <summary>Payload size: must be ≤ <paramref name="max"/> (0 allowed).</summary>
    public static void PayloadSize(long value, long max)
    {
        if (value < 0 || value > max)
        {
            throw new ValidationException("guest result limit exceeded");
        }
    }

    public static void EvalCode(string code)
    {
        if (string.IsNullOrWhiteSpace(code))
        {
            throw new ValidationException("eval code must not be empty");
        }

        if (Utf8Len(code) > MaxCodeBytes)
        {
            throw new ValidationException("eval code limit exceeded");
        }
    }

    /// <summary>Timeout in seconds: finite and strictly positive (mirror of Rust timeout_secs).</summary>
    public static void Timeout(double timeoutS)
    {
        if (double.IsNaN(timeoutS) || double.IsInfinity(timeoutS) || timeoutS <= 0)
        {
            throw new ValidationException("timeout must be a positive number of seconds");
        }
    }

    public static void EvalTimeout(double? timeoutS)
    {
        if (timeoutS.HasValue)
        {
            Timeout(timeoutS.Value);
        }
    }

    /// <summary>Sandbox / cancel id: non-empty, ≤ 128 chars, ASCII [A-Za-z0-9_-] only (PROTOCOL.md §1.1).</summary>
    public static void Id(string id)
    {
        if (id.Length == 0 || id.Length > 128)
        {
            throw new ValidationException("invalid forkd sandbox id");
        }

        foreach (var c in id)
        {
            if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')
                || c == '-' || c == '_'))
            {
                throw new ValidationException("invalid forkd sandbox id");
            }
        }
    }
}
