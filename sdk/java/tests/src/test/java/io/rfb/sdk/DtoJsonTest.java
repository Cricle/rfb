package io.rfb.sdk;

import com.fasterxml.jackson.databind.JsonNode;
import org.junit.jupiter.api.Test;

import java.nio.charset.StandardCharsets;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * DTO JSON field names must match PROTOCOL.md §1.3 exactly, and missing fields
 * must fall back to the controller defaults (serde(default)). The create-sandbox
 * request body wire names are covered by
 * {@link ControllerHttpTest#createSandboxSerializesExactFieldNames()}.
 */
class DtoJsonTest {
    private static JsonNode parse(String json) {
        return io.rfb.sdk.internal.Json.parse(json.getBytes(StandardCharsets.UTF_8));
    }

    @Test
    void snapshotInfoDeserializesWithDefaults() {
        Snapshot snapshot = io.rfb.sdk.internal.Json.MAPPER
                .convertValue(parse("{\"tag\":\"base\"}"), Snapshot.class);
        assertEquals("base", snapshot.tag);
        assertEquals("", snapshot.dir);
        assertEquals("", snapshot.status);
        assertFalse(snapshot.bootable);
        assertNull(snapshot.createdAtUnix);
        assertNull(snapshot.branchedFrom);
        assertNull(snapshot.pauseMs);
        assertNull(snapshot.diffMs);
        assertNull(snapshot.diffPhysicalBytes);
        assertNull(snapshot.diffLogicalBytes);
        assertNull(snapshot.warning);
        assertNull(snapshot.digest);
        assertNull(snapshot.provenance);
    }

    @Test
    void sandboxInfoDeserializesWithDefaults() {
        SandboxInfo info = io.rfb.sdk.internal.Json.MAPPER.convertValue(
                parse("{\"id\":\"sb-1\",\"snapshot_tag\":\"base\",\"guest_addr\":\"127.0.0.1:7021\"}"),
                SandboxInfo.class);
        assertEquals("sb-1", info.id);
        assertEquals("base", info.snapshotTag);
        assertEquals("127.0.0.1:7021", info.guestAddr);
        assertNull(info.netns);
        assertNull(info.createdAtUnix);
        assertNull(info.memoryLimitMib);
        assertNull(info.pid);
        assertFalse(info.hasBranched);
        assertEquals(0, info.branchCount);
    }

    @Test
    void snapshotRoundTripsAllSection13Fields() {
        Snapshot snapshot = io.rfb.sdk.internal.Json.MAPPER.convertValue(parse(
                "{\"tag\":\"base\",\"dir\":\"/snaps/base\",\"created_at_unix\":17,"
                        + "\"branched_from\":\"root\",\"pause_ms\":12,\"diff_ms\":34,"
                        + "\"diff_physical_bytes\":56,\"diff_logical_bytes\":78,\"warning\":\"w\","
                        + "\"status\":\"ready\",\"bootable\":true,\"digest\":\"sha256:abc\","
                        + "\"provenance\":{\"k\":1}}"), Snapshot.class);
        assertEquals("base", snapshot.tag);
        assertEquals("/snaps/base", snapshot.dir);
        assertEquals(Long.valueOf(17), snapshot.createdAtUnix);
        assertEquals("root", snapshot.branchedFrom);
        assertEquals(Long.valueOf(12), snapshot.pauseMs);
        assertEquals(Long.valueOf(34), snapshot.diffMs);
        assertEquals(Long.valueOf(56), snapshot.diffPhysicalBytes);
        assertEquals(Long.valueOf(78), snapshot.diffLogicalBytes);
        assertEquals("w", snapshot.warning);
        assertEquals("ready", snapshot.status);
        assertTrue(snapshot.bootable);
        assertEquals("sha256:abc", snapshot.digest);
        assertNotNull(snapshot.provenance);
    }

    @Test
    void sandboxInfoSerializesCamelFree() throws Exception {
        SandboxInfo info = new SandboxInfo();
        String json = io.rfb.sdk.internal.Json.MAPPER.writeValueAsString(info);
        assertFalse(json.contains("snapshotTag"));
        assertTrue(json.contains("snapshot_tag"));
        assertTrue(json.contains("guest_addr"));
        assertTrue(json.contains("has_branched"));
        assertTrue(json.contains("branch_count"));
    }
}
