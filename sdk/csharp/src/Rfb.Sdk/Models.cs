using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Rfb.Sdk;

/// <summary>Controller snapshot metadata (PROTOCOL.md §1.3; serde(default) semantics).</summary>
public sealed class Snapshot
{
    /// <summary>Snapshot tag (unique name).</summary>
    [JsonPropertyName("tag")]
    public string Tag { get; set; } = "";

    /// <summary>On-disk snapshot directory.</summary>
    [JsonPropertyName("dir")]
    public string Dir { get; set; } = "";

    /// <summary>Creation time (Unix seconds).</summary>
    [JsonPropertyName("created_at_unix")]
    public long? CreatedAtUnix { get; set; }

    /// <summary>Parent tag when this snapshot was branched.</summary>
    [JsonPropertyName("branched_from")]
    public string? BranchedFrom { get; set; }

    /// <summary>VM pause duration in milliseconds.</summary>
    [JsonPropertyName("pause_ms")]
    public long? PauseMs { get; set; }

    /// <summary>Branch diff duration in milliseconds.</summary>
    [JsonPropertyName("diff_ms")]
    public long? DiffMs { get; set; }

    /// <summary>Physical bytes written by the branch diff.</summary>
    [JsonPropertyName("diff_physical_bytes")]
    public long? DiffPhysicalBytes { get; set; }

    /// <summary>Logical bytes written by the branch diff.</summary>
    [JsonPropertyName("diff_logical_bytes")]
    public long? DiffLogicalBytes { get; set; }

    /// <summary>Optional controller warning for this snapshot.</summary>
    [JsonPropertyName("warning")]
    public string? Warning { get; set; }

    /// <summary>Lifecycle status (for example ready or failed).</summary>
    [JsonPropertyName("status")]
    public string Status { get; set; } = "";

    /// <summary>True once the snapshot can spawn sandboxes.</summary>
    [JsonPropertyName("bootable")]
    public bool Bootable { get; set; }

    /// <summary>Content digest reported by the controller.</summary>
    [JsonPropertyName("digest")]
    public string? Digest { get; set; }

    /// <summary>Raw provenance record (controller JSON).</summary>
    [JsonPropertyName("provenance")]
    public JsonElement? Provenance { get; set; }
}

/// <summary>Controller sandbox metadata (PROTOCOL.md §1.3).</summary>
public sealed class SandboxInfo
{
    /// <summary>Sandbox id (controller-assigned).</summary>
    [JsonPropertyName("id")]
    public string Id { get; set; } = "";

    /// <summary>Tag of the snapshot this sandbox was created from.</summary>
    [JsonPropertyName("snapshot_tag")]
    public string SnapshotTag { get; set; } = "";

    /// <summary>Network namespace (null for shared-tap sandboxes).</summary>
    [JsonPropertyName("netns")]
    public string? Netns { get; set; }

    /// <summary>Creation time (Unix seconds).</summary>
    [JsonPropertyName("created_at_unix")]
    public long? CreatedAtUnix { get; set; }

    /// <summary>Host:port of the guest agent (TCP).</summary>
    [JsonPropertyName("guest_addr")]
    public string GuestAddr { get; set; } = "";

    /// <summary>Memory cap in MiB (null = controller default).</summary>
    [JsonPropertyName("memory_limit_mib")]
    public long? MemoryLimitMib { get; set; }

    /// <summary>PID of the backing Firecracker process.</summary>
    [JsonPropertyName("pid")]
    public long? Pid { get; set; }

    /// <summary>True when this sandbox has produced a branch.</summary>
    [JsonPropertyName("has_branched")]
    public bool HasBranched { get; set; }

    /// <summary>Number of branches spawned from this sandbox.</summary>
    [JsonPropertyName("branch_count")]
    public long BranchCount { get; set; }
}

/// <summary>Unified result of exec/eval (UNIFIED_API.md §6).</summary>
public sealed record ExecResult(int ExitCode, byte[] Stdout, byte[] Stderr, bool TimedOut)
{
    /// <summary>stdout decoded as UTF-8 text.</summary>
    public string StdoutText => Encoding.UTF8.GetString(Stdout);
    /// <summary>stderr decoded as UTF-8 text.</summary>
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
    /// <summary>Process started.</summary>
    Started,
    /// <summary>stdout chunk.</summary>
    Stdout,
    /// <summary>stderr chunk.</summary>
    Stderr,
    /// <summary>Terminal frame; Code carries the exit code.</summary>
    Exit,
}

/// <summary>One event from an interactive guest stream (UNIFIED_API.md §5).</summary>
public sealed record StreamEvent(StreamEventKind Kind, byte[] Data, int? Code);
