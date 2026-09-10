package io.rfb.sdk;

import io.rfb.sdk.internal.Validation;
import org.junit.jupiter.api.Test;

import java.nio.charset.StandardCharsets;

import static org.junit.jupiter.api.Assertions.assertDoesNotThrow;
import static org.junit.jupiter.api.Assertions.assertThrows;

/** Every PROTOCOL.md §2.3 validation rule, enforced locally (fail closed). */
class ValidationTest {

    // ---- structured fs paths (ls/find/grep) ---------------------------------

    @Test
    void fsPathAcceptsRelativeAndWorkspacePrefix() {
        assertDoesNotThrow(() -> Validation.fsPath("."));
        assertDoesNotThrow(() -> Validation.fsPath("sub/dir/file.txt"));
        assertDoesNotThrow(() -> Validation.fsPath("/workspace"));
        assertDoesNotThrow(() -> Validation.fsPath("/workspace/sub"));
    }

    @Test
    void fsPathRejectsEmptyNulBackslashDotDotAndLong() {
        assertThrows(ValidationError.class, () -> Validation.fsPath(null));
        assertThrows(ValidationError.class, () -> Validation.fsPath(""));
        assertThrows(ValidationError.class, () -> Validation.fsPath("a\0b"));
        assertThrows(ValidationError.class, () -> Validation.fsPath("a\\b"));
        assertThrows(ValidationError.class, () -> Validation.fsPath("\\x"));
        assertThrows(ValidationError.class, () -> Validation.fsPath("a/../b"));
        assertThrows(ValidationError.class, () -> Validation.fsPath(".."));
        assertThrows(ValidationError.class, () -> Validation.fsPath("x".repeat(4097)));
    }

    @Test
    void fsPathRejectsAbsoluteOutsideWorkspace() {
        assertThrows(ValidationError.class, () -> Validation.fsPath("/etc"));
        assertThrows(ValidationError.class, () -> Validation.fsPath("/"));
        assertThrows(ValidationError.class, () -> Validation.fsPath("/workspacex"));
        assertThrows(ValidationError.class, () -> Validation.fsPath("/workspace/../x")); // ".." segment
    }

    @Test
    void filePathRejectsDrivePrefixButAllowsAbsolute() {
        assertThrows(ValidationError.class, () -> Validation.filePath("C:/x"));
        assertThrows(ValidationError.class, () -> Validation.filePath("C:\\x"));
        assertDoesNotThrow(() -> Validation.filePath("/abs/path"));
        assertDoesNotThrow(() -> Validation.filePath("rel/path"));
        assertThrows(ValidationError.class, () -> Validation.filePath("/abs/../esc"));
        assertThrows(ValidationError.class, () -> Validation.filePath("a\\b"));
        assertThrows(ValidationError.class, () -> Validation.filePath("x".repeat(4097)));
        assertThrows(ValidationError.class, () -> Validation.filePath(""));
    }

    // ---- patterns -------------------------------------------------------------

    @Test
    void patternRejectsEmptyNulAndOversize() {
        assertThrows(ValidationError.class, () -> Validation.pattern(null));
        assertThrows(ValidationError.class, () -> Validation.pattern(""));
        assertThrows(ValidationError.class, () -> Validation.pattern("a\0b"));
        assertThrows(ValidationError.class, () -> Validation.pattern("x".repeat(1025)));
        assertDoesNotThrow(() -> Validation.pattern("x".repeat(1024)));
    }

    // ---- limits ----------------------------------------------------------------

    @Test
    void limitRejectsZeroAndOversize() {
        assertThrows(ValidationError.class, () -> Validation.limit(0, Validation.MAX_GUEST_RESULTS));
        assertThrows(ValidationError.class, () -> Validation.limit(1001, Validation.MAX_GUEST_RESULTS));
        assertThrows(ValidationError.class, () -> Validation.limit(51201, Validation.MAX_GUEST_RESULT_BYTES));
        assertThrows(ValidationError.class, () -> Validation.limit(-1, Validation.MAX_GUEST_RESULTS));
        assertDoesNotThrow(() -> Validation.limit(1, Validation.MAX_GUEST_RESULTS));
        assertDoesNotThrow(() -> Validation.limit(1000, Validation.MAX_GUEST_RESULTS));
        assertDoesNotThrow(() -> Validation.limit(51200, Validation.MAX_GUEST_RESULT_BYTES));
    }

    @Test
    void payloadSizeRejectsOverMax() {
        assertThrows(ValidationError.class, () -> Validation.payloadSize(Validation.MAX_GUEST_CODE_BYTES + 1,
                Validation.MAX_GUEST_CODE_BYTES));
        assertDoesNotThrow(() -> Validation.payloadSize(0, Validation.MAX_GUEST_RESULT_BYTES));
    }

    // ---- eval -------------------------------------------------------------------

    @Test
    void evalCodeRejectsBlankAndOversize() {
        assertThrows(ValidationError.class, () -> Validation.evalCode(null));
        assertThrows(ValidationError.class, () -> Validation.evalCode(""));
        assertThrows(ValidationError.class, () -> Validation.evalCode("  \n\t "));
        assertThrows(ValidationError.class, () -> Validation.evalCode("x".repeat(Validation.MAX_GUEST_CODE_BYTES + 1)));
        assertDoesNotThrow(() -> Validation.evalCode("print(1)"));
        // 1 MiB exactly is allowed (byte length, not chars)
        String exactly1MiB = "a".repeat(Validation.MAX_GUEST_CODE_BYTES);
        assertDoesNotThrow(() -> Validation.evalCode(exactly1MiB));
    }

    @Test
    void evalTimeoutRejectsZero() {
        assertThrows(ValidationError.class, () -> Validation.evalTimeoutSeconds(0));
        assertDoesNotThrow(() -> Validation.evalTimeoutSeconds(1));
    }

    // ---- ids --------------------------------------------------------------------

    @Test
    void cancelIdAndSandboxIdRules() {
        assertThrows(ValidationError.class, () -> Validation.cancelId(null));
        assertThrows(ValidationError.class, () -> Validation.cancelId(""));
        assertThrows(ValidationError.class, () -> Validation.cancelId("a".repeat(129)));
        assertThrows(ValidationError.class, () -> Validation.cancelId("has space"));
        assertThrows(ValidationError.class, () -> Validation.cancelId("has/slash"));
        assertThrows(ValidationError.class, () -> Validation.cancelId("has.dot"));
        assertDoesNotThrow(() -> Validation.cancelId("a".repeat(128)));
        assertDoesNotThrow(() -> Validation.cancelId("AbZ09-_"));

        assertThrows(ValidationError.class, () -> Validation.sandboxId(""));
        assertThrows(ValidationError.class, () -> Validation.sandboxId("a".repeat(129)));
        assertThrows(ValidationError.class, () -> Validation.sandboxId("../escape"));
        assertDoesNotThrow(() -> Validation.sandboxId("sb-1_2"));
    }

    @Test
    void byteLengthsAreUtf8NotChars() {
        // 4097 bytes but fewer chars: multi-byte UTF-8 must count as bytes
        String multiByte = "é".repeat(2049); // 2 bytes each → 4098 bytes
        assertThrows(ValidationError.class, () -> Validation.fsPath(multiByte));
        String multiBytePattern = "é".repeat(513); // 1026 bytes
        assertThrows(ValidationError.class, () -> Validation.pattern(multiBytePattern));
        assertEqualsUtf8Bytes("é", 2);
    }

    private static void assertEqualsUtf8Bytes(String s, int expected) {
        org.junit.jupiter.api.Assertions.assertEquals(expected, s.getBytes(StandardCharsets.UTF_8).length);
    }
}
