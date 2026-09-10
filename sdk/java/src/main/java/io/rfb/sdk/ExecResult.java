package io.rfb.sdk;

/**
 * Unified result of {@code exec} and {@code eval} (UNIFIED_API.md §6).
 * {@code exitCode} may be null when the guest did not report a status.
 */
public final class ExecResult {
    public final Integer exitCode;
    public final byte[] stdout;
    public final byte[] stderr;
    public final boolean timedOut;

    public ExecResult(Integer exitCode, byte[] stdout, byte[] stderr, boolean timedOut) {
        this.exitCode = exitCode;
        this.stdout = stdout == null ? new byte[0] : stdout;
        this.stderr = stderr == null ? new byte[0] : stderr;
        this.timedOut = timedOut;
    }

    /** Stdout decoded as UTF-8. */
    public String stdoutText() {
        return new String(stdout, java.nio.charset.StandardCharsets.UTF_8);
    }

    /** Stderr decoded as UTF-8. */
    public String stderrText() {
        return new String(stderr, java.nio.charset.StandardCharsets.UTF_8);
    }

    @Override
    public String toString() {
        return "ExecResult{exitCode=" + exitCode + ", timedOut=" + timedOut
                + ", stdout=" + stdout.length + "B, stderr=" + stderr.length + "B}";
    }
}
