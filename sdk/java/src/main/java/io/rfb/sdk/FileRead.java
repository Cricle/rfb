package io.rfb.sdk;

import java.util.Arrays;
import java.util.Objects;

/** Result of a guest file read (UNIFIED_API.md §6). The byte array is copied in and out. */
public final class FileRead {
    private final byte[] data;
    private final boolean truncated;
    private final Long totalBytes;

    public FileRead(byte[] data, boolean truncated, Long totalBytes) {
        this.data = data == null ? new byte[0] : data.clone();
        this.truncated = truncated;
        this.totalBytes = totalBytes;
    }

    /** A copy of the read bytes. */
    public byte[] getData() {
        return data.clone();
    }

    /** True when the read hit the requested cap before EOF. */
    public boolean isTruncated() {
        return truncated;
    }

    /** Total file size in bytes, when known. */
    public Long getTotalBytes() {
        return totalBytes;
    }

    @Override
    public boolean equals(Object other) {
        if (this == other) {
            return true;
        }
        if (!(other instanceof FileRead)) {
            return false;
        }
        FileRead that = (FileRead) other;
        return truncated == that.truncated
                && Arrays.equals(data, that.data)
                && Objects.equals(totalBytes, that.totalBytes);
    }

    @Override
    public int hashCode() {
        return Objects.hash(Arrays.hashCode(data), truncated, totalBytes);
    }
}
