package io.rfb.sdk;

/** One directory entry returned by {@code Sandbox.ls}. */
public final class DirEntry {
    public final String name;
    public final boolean isDir;
    /** Entry size in bytes, when known; null otherwise. */
    public final Long size;

    public DirEntry(String name, boolean isDir, Long size) {
        this.name = name;
        this.isDir = isDir;
        this.size = size;
    }
}
