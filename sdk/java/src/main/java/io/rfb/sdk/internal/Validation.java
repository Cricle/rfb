package io.rfb.sdk.internal;

import io.rfb.sdk.ValidationError;

import java.nio.charset.StandardCharsets;

/**
 * Local (fail-closed) request validation mirroring PROTOCOL.md §2.3 and
 * {@code rfb/src/guest/limits.rs}. All sizes are UTF-8 byte lengths. INTERNAL —
 * not part of the public API.
 */
public final class Validation {
    public static final int MAX_GUEST_PATH_BYTES = 4096;
    public static final int MAX_GUEST_PATTERN_BYTES = 1024;
    public static final int MAX_GUEST_RESULTS = 1000;
    public static final int MAX_GUEST_RESULT_BYTES = 50 * 1024;
    public static final int MAX_GUEST_CODE_BYTES = 1024 * 1024;
    public static final int MAX_LINE_BYTES = 1024 * 1024;

    private Validation() {
    }

    /**
     * Structured fs path (ls/find/grep): non-empty, &le; 4096 bytes, no NUL, no
     * backslash anywhere, absolute only with the {@code /workspace} prefix, and
     * no {@code ..} segment.
     */
    public static void fsPath(String path) {
        if (badBasePath(path) || (path.startsWith("/") && !isWorkspacePath(path))) {
            throw new ValidationError(
                    "invalid guest fs path: must be a non-empty, relative, non-escaping guest path");
        }
    }

    /** Absolute fs paths are only allowed under the {@code /workspace} root. */
    private static boolean isWorkspacePath(String path) {
        return path.equals("/workspace") || path.startsWith("/workspace/");
    }

    /**
     * File path (read/write targets and eval/stream cwd): non-empty, &le; 4096
     * bytes, no NUL, no backslash, no {@code X:} drive prefix, no {@code ..}
     * segment (absolute or relative both allowed).
     */
    public static void filePath(String path) {
        if (badBasePath(path) || (path.length() >= 2 && path.charAt(1) == ':')) {
            throw new ValidationError(
                    "invalid guest file path: must be a non-empty, non-escaping guest path");
        }
    }

    /** find/grep pattern: non-empty, no NUL, &le; 1024 bytes. */
    public static void pattern(String pattern) {
        if (pattern == null || pattern.isEmpty() || pattern.indexOf('\0') >= 0) {
            throw new ValidationError("invalid guest pattern: must be non-empty and NUL-free");
        }
        if (pattern.getBytes(StandardCharsets.UTF_8).length > MAX_GUEST_PATTERN_BYTES) {
            throw new ValidationError("guest pattern exceeds " + MAX_GUEST_PATTERN_BYTES + " bytes");
        }
    }

    /** Count limit (max_results / max_bytes): must be &gt; 0 and &le; max. */
    public static void limit(long value, long max) {
        if (value <= 0 || value > max) {
            throw new ValidationError("guest limit exceeded: value must be > 0 and <= " + max);
        }
    }

    /** Payload size limit (write data / eval code). */
    public static void payloadSize(long value, long max) {
        if (value > max) {
            throw new ValidationError("guest payload exceeds " + max + " bytes");
        }
    }

    /** eval code: non-empty after trimming whitespace, &le; 1 MiB. */
    public static void evalCode(String code) {
        if (code == null || code.trim().isEmpty()) {
            throw new ValidationError("eval code must not be empty");
        }
        if (code.getBytes(StandardCharsets.UTF_8).length > MAX_GUEST_CODE_BYTES) {
            throw new ValidationError("eval code exceeds " + MAX_GUEST_CODE_BYTES + " bytes");
        }
    }

    /** eval timeout in seconds: must not be zero. */
    public static void evalTimeoutSeconds(long timeoutSeconds) {
        if (timeoutSeconds == 0) {
            throw new ValidationError("eval timeout must not be zero");
        }
    }

    /** Optional cancel id: non-empty, &le; 128, only [A-Za-z0-9_-]. */
    public static void cancelId(String id) {
        if (!isValidId(id)) {
            throw new ValidationError("invalid cancel id: must be non-empty, <= 128 chars, [A-Za-z0-9_-]");
        }
    }

    /** forkd sandbox id used in URL paths: non-empty, &le; 128, only [A-Za-z0-9_-]. */
    public static void sandboxId(String id) {
        if (!isValidId(id)) {
            throw new ValidationError("invalid forkd sandbox id");
        }
    }

    private static boolean isValidId(String id) {
        if (id == null || id.isEmpty() || id.length() > 128) {
            return false;
        }
        for (int i = 0; i < id.length(); i++) {
            char c = id.charAt(i);
            boolean ok = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
                    || (c >= '0' && c <= '9') || c == '-' || c == '_';
            if (!ok) {
                return false;
            }
        }
        return true;
    }

    private static boolean badBasePath(String path) {
        if (path == null || path.isEmpty()) {
            return true;
        }
        byte[] bytes = path.getBytes(StandardCharsets.UTF_8);
        if (bytes.length > MAX_GUEST_PATH_BYTES) {
            return true;
        }
        for (byte b : bytes) {
            if (b == 0 || b == '\\') {
                return true;
            }
        }
        for (String segment : path.split("/", -1)) {
            if ("..".equals(segment)) {
                return true;
            }
        }
        return false;
    }
}
