package io.rfb.sdk;

/** Result of a guest file read (UNIFIED_API.md §6). */
public final class FileRead {
    public final byte[] data;
    public final boolean truncated;
    /** Total file size when the backend knows it; null otherwise. */
    public final Long totalBytes;

    public FileRead(byte[] data, boolean truncated, Long totalBytes) {
        this.data = data == null ? new byte[0] : data;
        this.truncated = truncated;
        this.totalBytes = totalBytes;
    }
}
