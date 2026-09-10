package io.rfb.sdk;

/** One grep match with optional location. */
public final class GrepMatch {
    public final String path;
    /** 1-based line number, when known; null otherwise. */
    public final Long line;
    /** 1-based column number, when known; null otherwise. */
    public final Long column;
    public final String text;

    public GrepMatch(String path, Long line, Long column, String text) {
        this.path = path;
        this.line = line;
        this.column = column;
        this.text = text;
    }
}
