package io.rfb.sdk;

import java.util.Objects;

/** One grep match with optional location (UNIFIED_API.md §6). */
public final class GrepMatch {
    private final String path;
    private final Long line;
    private final Long column;
    private final String text;

    public GrepMatch(String path, Long line, Long column, String text) {
        this.path = path;
        this.line = line;
        this.column = column;
        this.text = text;
    }

    /** Guest file path of the match. */
    public String getPath() {
        return path;
    }

    /** 1-based line number, when reported. */
    public Long getLine() {
        return line;
    }

    /** 1-based column, when reported. */
    public Long getColumn() {
        return column;
    }

    /** Matched line text. */
    public String getText() {
        return text;
    }

    @Override
    public boolean equals(Object other) {
        if (this == other) {
            return true;
        }
        if (!(other instanceof GrepMatch)) {
            return false;
        }
        GrepMatch that = (GrepMatch) other;
        return Objects.equals(path, that.path)
                && Objects.equals(line, that.line)
                && Objects.equals(column, that.column)
                && Objects.equals(text, that.text);
    }

    @Override
    public int hashCode() {
        return Objects.hash(path, line, column, text);
    }
}
