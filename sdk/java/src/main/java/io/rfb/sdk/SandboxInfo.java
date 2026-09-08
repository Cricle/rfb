package io.rfb.sdk;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;

/** Metadata describing a live forkd sandbox (PROTOCOL.md §1.3). */
@JsonIgnoreProperties(ignoreUnknown = true)
public class SandboxInfo {
    @JsonProperty("id")
    public String id = "";

    @JsonProperty("snapshot_tag")
    public String snapshotTag = "";

    @JsonProperty("netns")
    public String netns;

    @JsonProperty("created_at_unix")
    public Long createdAtUnix;

    @JsonProperty("guest_addr")
    public String guestAddr = "";

    @JsonProperty("memory_limit_mib")
    public Long memoryLimitMib;

    @JsonProperty("pid")
    public Long pid;

    @JsonProperty("has_branched")
    public boolean hasBranched = false;

    @JsonProperty("branch_count")
    public long branchCount = 0;
}
