package io.rfb.sdk;

import java.util.Arrays;
import java.util.Objects;

/**
 * Unified result of {@code exec} and {@code eval} (UNIFIED_API.md §6).
 * {@code exitCode} may be null when the guest did not report a status.
 * Byte arrays are copied in and out so the value stays effectively immutable.
 */
public final class ExecResult {
    private final Integer exitCode;
    private final byte[] stdout;
    private final byte[] stderr;
    private final boolean timedOut;

    public ExecResult(Integer exitCode, byte[] stdout, byte[] stderr, boolean timedOut) {
        this.exitCode = exitCode;
        this.stdout = stdout == null ? new byte[0] : stdout.clone();
        this.stderr = stderr == null ? new byte[0] : stderr.clone();
        this.timedOut = timedOut;
    }

    /** Exit code reported by the guest; null when unreported. */
    public Integer getExitCode() {
        return exitCode;
    }

    /** A copy of the captured stdout bytes. */
    public byte[] getStdout() {
        return stdout.clone();
    }

    /** A copy of the captured stderr bytes. */
    public byte[] getStderr() {
        return stderr.clone();
    }

    /** True when the request hit its deadline. */
    public boolean isTimedOut() {
        return timedOut;
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
    public boolean equals(Object other) {
        if (this == other) {
            return true;
        }
        if (!(other instanceof ExecResult)) {
            return false;
        }
        ExecResult that = (ExecResult) other;
        return timedOut == that.timedOut
                && Objects.equals(exitCode, that.exitCode)
                && Arrays.equals(stdout, that.stdout)
                && Arrays.equals(stderr, that.stderr);
    }

    @Override
    public int hashCode() {
        return Objects.hash(exitCode, timedOut, Arrays.hashCode(stdout), Arrays.hashCode(stderr));
    }

    @Override
    public String toString() {
        return "ExecResult{exitCode=" + exitCode + ", timedOut=" + timedOut
                + ", stdout=" + stdout.length + "B, stderr=" + stderr.length + "B}";
    }
}
