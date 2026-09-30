package io.rfb.sdk;

import java.util.Objects;

/** One directory entry returned by {@code Sandbox.ls}. */
public final class DirEntry {
    private final String name;
    private final boolean isDir;
    private final Long size;

    public DirEntry(String name, boolean isDir, Long size) {
        this.name = name;
        this.isDir = isDir;
        this.size = size;
    }

    /** Entry name. */
    public String getName() {
        return name;
    }

    /** True when the entry is a directory. */
    public boolean isDir() {
        return isDir;
    }

    /** Entry size in bytes, when known; null otherwise. */
    public Long getSize() {
        return size;
    }

    @Override
    public boolean equals(Object other) {
        if (this == other) {
            return true;
        }
        if (!(other instanceof DirEntry)) {
            return false;
        }
        DirEntry that = (DirEntry) other;
        return isDir == that.isDir
                && Objects.equals(name, that.name)
                && Objects.equals(size, that.size);
    }

    @Override
    public int hashCode() {
        return Objects.hash(name, isDir, size);
    }

    @Override
    public String toString() {
        return "DirEntry{name=" + name + ", isDir=" + isDir + ", size=" + size + "}";
    }
}
