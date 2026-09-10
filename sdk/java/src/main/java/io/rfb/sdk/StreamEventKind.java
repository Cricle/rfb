package io.rfb.sdk;

/** Kind of a {@link StreamEvent} (UNIFIED_API.md §5). */
public enum StreamEventKind {
    /** The guest acknowledged the stream and is ready to accept input. */
    STARTED,
    /** A chunk of standard output (carried in {@link StreamEvent#data}). */
    STDOUT,
    /** A chunk of standard error (carried in {@link StreamEvent#data}). */
    STDERR,
    /** The stream terminated (exit code in {@link StreamEvent#code}, null when unreported). */
    EXIT
}
