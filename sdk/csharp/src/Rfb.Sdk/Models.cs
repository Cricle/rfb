using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Rfb.Sdk;

/// <summary>Controller snapshot metadata (PROTOCOL.md §1.3; serde(default) semantics).</summary>
public sealed class Snapshot
{
    [JsonPropertyName("tag")]
    public string Tag { get; set; } = "";

    [JsonPropertyName("dir")]
    public string Dir { get; set; } = "";

    [JsonPropertyName("created_at_unix")]
    public long? CreatedAtUnix { get; set; }

    [JsonPropertyName("branched_from")]
    public string? BranchedFrom { get; set; }

    [JsonPropertyName("pause_ms")]
    public long? PauseMs { get; set; }

    [JsonPropertyName("diff_ms")]
    public long? DiffMs { get; set; }

    [JsonPropertyName("diff_physical_bytes")]
    public long? DiffPhysicalBytes { get; set; }

    [JsonPropertyName("diff_logical_bytes")]
    public long? DiffLogicalBytes { get; set; }

    [JsonPropertyName("warning")]
    public string? Warning { get; set; }

    [JsonPropertyName("status")]
    public string Status { get; set; } = "";

    [JsonPropertyName("bootable")]
    public bool Bootable { get; set; }

    [JsonPropertyName("digest")]
    public string? Digest { get; set; }

    [JsonPropertyName("provenance")]
    public JsonElement? Provenance { get; set; }
}

/// <summary>Controller sandbox metadata (PROTOCOL.md §1.3).</summary>
public sealed class SandboxInfo
{
    [JsonPropertyName("id")]
    public string Id { get; set; } = "";

    [JsonPropertyName("snapshot_tag")]
    public string SnapshotTag { get; set; } = "";

    [JsonPropertyName("netns")]
    public string? Netns { get; set; }

    [JsonPropertyName("created_at_unix")]
    public long? CreatedAtUnix { get; set; }

    [JsonPropertyName("guest_addr")]
    public string GuestAddr { get; set; } = "";

    [JsonPropertyName("memory_limit_mib")]
    public long? MemoryLimitMib { get; set; }

    [JsonPropertyName("pid")]
    public long? Pid { get; set; }

    [JsonPropertyName("has_branched")]
    public bool HasBranched { get; set; }

    [JsonPropertyName("branch_count")]
    public long BranchCount { get; set; }
}

/// <summary>Unified result of exec/eval (UNIFIED_API.md §6).</summary>
public sealed record ExecResult(int ExitCode, byte[] Stdout, byte[] Stderr, bool TimedOut)
{
    public string StdoutText => Encoding.UTF8.GetString(Stdout);
    public string StderrText => Encoding.UTF8.GetString(Stderr);
}

/// <summary>One directory entry returned by ls (UNIFIED_API.md §6).</summary>
public sealed record DirEntry(string Name, bool IsDir, long? Size);

/// <summary>One grep match with optional location (UNIFIED_API.md §6).</summary>
public sealed record GrepMatch(string Path, long? Line, long? Column, string Text);

/// <summary>Result of a guest file read (UNIFIED_API.md §6).</summary>
public sealed record FileRead(byte[] Data, bool Truncated, long? TotalBytes);

/// <summary>Stream event kind: started | stdout | stderr | exit.</summary>
public enum StreamEventKind
{
    Started,
    Stdout,
    Stderr,
    Exit,
}

/// <summary>One event from an interactive guest stream (UNIFIED_API.md §5).</summary>
public sealed record StreamEvent(StreamEventKind Kind, byte[] Data, int? Code);
