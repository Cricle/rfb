package io.rfb.sdk;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;
import java.util.Objects;

/**
 * Metadata describing a forkd snapshot (PROTOCOL.md §1.3). Missing JSON fields
 * fall back to the controller's own defaults ({@code serde(default)}).
 */
@JsonIgnoreProperties(ignoreUnknown = true)
public class Snapshot {
    @JsonProperty("tag")
    private String tag = "";

    @JsonProperty("dir")
    private String dir = "";

    @JsonProperty("created_at_unix")
    private Long createdAtUnix;

    @JsonProperty("branched_from")
    private String branchedFrom;

    @JsonProperty("pause_ms")
    private Long pauseMs;

    @JsonProperty("diff_ms")
    private Long diffMs;

    @JsonProperty("diff_physical_bytes")
    private Long diffPhysicalBytes;

    @JsonProperty("diff_logical_bytes")
    private Long diffLogicalBytes;

    @JsonProperty("warning")
    private String warning;

    @JsonProperty("status")
    private String status = "";

    @JsonProperty("bootable")
    private boolean bootable = false;

    @JsonProperty("digest")
    private String digest;

    @JsonProperty("provenance")
    private Object provenance;

    /** Snapshot tag (unique name). */
    @JsonProperty("tag")
    public String getTag() {
        return tag;
    }

    /** On-disk snapshot directory. */
    @JsonProperty("dir")
    public String getDir() {
        return dir;
    }

    /** Creation time (Unix seconds). */
    @JsonProperty("created_at_unix")
    public Long getCreatedAtUnix() {
        return createdAtUnix;
    }

    /** Parent tag when this snapshot was branched. */
    @JsonProperty("branched_from")
    public String getBranchedFrom() {
        return branchedFrom;
    }

    /** VM pause duration in milliseconds. */
    @JsonProperty("pause_ms")
    public Long getPauseMs() {
        return pauseMs;
    }

    /** Branch diff duration in milliseconds. */
    @JsonProperty("diff_ms")
    public Long getDiffMs() {
        return diffMs;
    }

    /** Physical bytes written by the branch diff. */
    @JsonProperty("diff_physical_bytes")
    public Long getDiffPhysicalBytes() {
        return diffPhysicalBytes;
    }

    /** Logical bytes written by the branch diff. */
    @JsonProperty("diff_logical_bytes")
    public Long getDiffLogicalBytes() {
        return diffLogicalBytes;
    }

    /** Optional controller warning. */
    @JsonProperty("warning")
    public String getWarning() {
        return warning;
    }

    /** Lifecycle status (for example ready or failed). */
    @JsonProperty("status")
    public String getStatus() {
        return status;
    }

    /** True once the snapshot can spawn sandboxes. */
    @JsonProperty("bootable")
    public boolean isBootable() {
        return bootable;
    }

    /** Content digest reported by the controller. */
    @JsonProperty("digest")
    public String getDigest() {
        return digest;
    }

    /** Raw provenance record (controller JSON). */
    @JsonProperty("provenance")
    public Object getProvenance() {
        return provenance;
    }

    @Override
    public boolean equals(Object other) {
        if (this == other) {
            return true;
        }
        if (!(other instanceof Snapshot)) {
            return false;
        }
        Snapshot that = (Snapshot) other;
        return bootable == that.bootable
                && Objects.equals(tag, that.tag)
                && Objects.equals(dir, that.dir)
                && Objects.equals(createdAtUnix, that.createdAtUnix)
                && Objects.equals(branchedFrom, that.branchedFrom)
                && Objects.equals(pauseMs, that.pauseMs)
                && Objects.equals(diffMs, that.diffMs)
                && Objects.equals(diffPhysicalBytes, that.diffPhysicalBytes)
                && Objects.equals(diffLogicalBytes, that.diffLogicalBytes)
                && Objects.equals(warning, that.warning)
                && Objects.equals(status, that.status)
                && Objects.equals(digest, that.digest)
                && Objects.equals(provenance, that.provenance);
    }

    @Override
    public int hashCode() {
        return Objects.hash(tag, dir, createdAtUnix, branchedFrom, pauseMs, diffMs,
                diffPhysicalBytes, diffLogicalBytes, warning, status, bootable, digest, provenance);
    }

    @Override
    public String toString() {
        return "Snapshot{tag=" + tag + ", status=" + status + ", bootable=" + bootable + "}";
    }
}
