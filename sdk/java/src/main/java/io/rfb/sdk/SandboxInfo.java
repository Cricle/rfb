package io.rfb.sdk;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;
import java.util.Objects;

/** Controller sandbox metadata (PROTOCOL.md §1.3). */
@JsonIgnoreProperties(ignoreUnknown = true)
public class SandboxInfo {
    @JsonProperty("id")
    private String id = "";

    @JsonProperty("snapshot_tag")
    private String snapshotTag = "";

    @JsonProperty("netns")
    private String netns;

    @JsonProperty("created_at_unix")
    private Long createdAtUnix;

    @JsonProperty("guest_addr")
    private String guestAddr = "";

    @JsonProperty("memory_limit_mib")
    private Long memoryLimitMib;

    @JsonProperty("pid")
    private Long pid;

    @JsonProperty("has_branched")
    private boolean hasBranched = false;

    @JsonProperty("branch_count")
    private long branchCount = 0L;

    /** Sandbox id (controller-assigned). */
    @JsonProperty("id")
    public String getId() {
        return id;
    }

    /** Sets the sandbox id. */
    @JsonProperty("id")
    public void setId(String id) {
        this.id = id == null ? "" : id;
    }

    /** Tag of the snapshot this sandbox was created from. */
    @JsonProperty("snapshot_tag")
    public String getSnapshotTag() {
        return snapshotTag;
    }

    /** Sets the snapshot tag. */
    @JsonProperty("snapshot_tag")
    public void setSnapshotTag(String snapshotTag) {
        this.snapshotTag = snapshotTag == null ? "" : snapshotTag;
    }

    /** Network namespace (null for shared-tap sandboxes). */
    @JsonProperty("netns")
    public String getNetns() {
        return netns;
    }

    /** Sets the network namespace. */
    @JsonProperty("netns")
    public void setNetns(String netns) {
        this.netns = netns; // nullable: intentionally accepts null
    }

    /** Creation time (Unix seconds). */
    @JsonProperty("created_at_unix")
    public Long getCreatedAtUnix() {
        return createdAtUnix;
    }

    /** Sets the creation time (Unix seconds). */
    @JsonProperty("created_at_unix")
    public void setCreatedAtUnix(Long createdAtUnix) {
        this.createdAtUnix = createdAtUnix; // nullable
    }

    /** Host:port of the guest agent (TCP). */
    @JsonProperty("guest_addr")
    public String getGuestAddr() {
        return guestAddr;
    }

    /** Sets the guest agent address. */
    @JsonProperty("guest_addr")
    public void setGuestAddr(String guestAddr) {
        this.guestAddr = guestAddr == null ? "" : guestAddr;
    }

    /** Memory cap in MiB (null = controller default). */
    @JsonProperty("memory_limit_mib")
    public Long getMemoryLimitMib() {
        return memoryLimitMib;
    }

    /** Sets the memory cap in MiB. */
    @JsonProperty("memory_limit_mib")
    public void setMemoryLimitMib(Long memoryLimitMib) {
        this.memoryLimitMib = memoryLimitMib; // nullable
    }

    /** PID of the backing Firecracker process. */
    @JsonProperty("pid")
    public Long getPid() {
        return pid;
    }

    /** Sets the backing Firecracker PID. */
    @JsonProperty("pid")
    public void setPid(Long pid) {
        this.pid = pid; // nullable
    }

    /** True when this sandbox has produced a branch. */
    @JsonProperty("has_branched")
    public boolean isHasBranched() {
        return hasBranched;
    }

    /** Sets the branched flag. */
    @JsonProperty("has_branched")
    public void setHasBranched(boolean hasBranched) {
        this.hasBranched = hasBranched;
    }

    /** Number of branches spawned from this sandbox. */
    @JsonProperty("branch_count")
    public long getBranchCount() {
        return branchCount;
    }

    /** Sets the branch count. */
    @JsonProperty("branch_count")
    public void setBranchCount(long branchCount) {
        this.branchCount = branchCount;
    }

    @Override
    public boolean equals(Object other) {
        if (this == other) {
            return true;
        }
        if (!(other instanceof SandboxInfo)) {
            return false;
        }
        SandboxInfo that = (SandboxInfo) other;
        return hasBranched == that.hasBranched
                && branchCount == that.branchCount
                && Objects.equals(id, that.id)
                && Objects.equals(snapshotTag, that.snapshotTag)
                && Objects.equals(netns, that.netns)
                && Objects.equals(createdAtUnix, that.createdAtUnix)
                && Objects.equals(guestAddr, that.guestAddr)
                && Objects.equals(memoryLimitMib, that.memoryLimitMib)
                && Objects.equals(pid, that.pid);
    }

    @Override
    public int hashCode() {
        return Objects.hash(id, snapshotTag, netns, createdAtUnix, guestAddr,
                memoryLimitMib, pid, hasBranched, branchCount);
    }

    @Override
    public String toString() {
        return "SandboxInfo{id=" + id + ", snapshotTag=" + snapshotTag
                + ", guestAddr=" + guestAddr + "}";
    }
}
