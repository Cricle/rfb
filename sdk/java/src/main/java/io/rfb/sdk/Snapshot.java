package io.rfb.sdk;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;

/**
 * Metadata describing a forkd snapshot (PROTOCOL.md §1.3). Missing JSON fields
 * fall back to the controller's own defaults ({@code serde(default)}).
 */
@JsonIgnoreProperties(ignoreUnknown = true)
public class Snapshot {
    @JsonProperty("tag")
    public String tag = "";

    @JsonProperty("dir")
    public String dir = "";

    @JsonProperty("created_at_unix")
    public Long createdAtUnix;

    @JsonProperty("branched_from")
    public String branchedFrom;

    @JsonProperty("pause_ms")
    public Long pauseMs;

    @JsonProperty("diff_ms")
    public Long diffMs;

    @JsonProperty("diff_physical_bytes")
    public Long diffPhysicalBytes;

    @JsonProperty("diff_logical_bytes")
    public Long diffLogicalBytes;

    @JsonProperty("warning")
    public String warning;

    @JsonProperty("status")
    public String status = "";

    @JsonProperty("bootable")
    public boolean bootable = false;

    @JsonProperty("digest")
    public String digest;

    @JsonProperty("provenance")
    public Object provenance;
}
