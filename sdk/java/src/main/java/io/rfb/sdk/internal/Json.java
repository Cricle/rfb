package io.rfb.sdk.internal;

import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.rfb.sdk.DecodeError;

import java.io.IOException;
import java.nio.charset.StandardCharsets;

/**
 * Shared Jackson {@link ObjectMapper}. Unknown response fields are ignored
 * (mirroring serde's tolerant forward compatibility). INTERNAL — not part of
 * the public API.
 */
public final class Json {
    public static final ObjectMapper MAPPER = new ObjectMapper()
            .configure(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES, false);

    private Json() {
    }

    public static JsonNode parse(byte[] body) {
        try {
            return MAPPER.readTree(body);
        } catch (IOException e) {
            throw new DecodeError("invalid JSON: " + e.getMessage(), e);
        }
    }

    public static JsonNode parse(byte[] body, int offset, int length) {
        try {
            return MAPPER.readTree(body, offset, length);
        } catch (IOException e) {
            throw new DecodeError("invalid JSON: " + e.getMessage(), e);
        }
    }

    public static ObjectNode object() {
        return MAPPER.createObjectNode();
    }

    /** Encode bytes as a JSON number array (wire form of {@code Vec<u8>}). */
    public static ArrayNode bytesNode(byte[] bytes) {
        ArrayNode array = MAPPER.createArrayNode();
        for (byte b : bytes) {
            array.add(b & 0xFF);
        }
        return array;
    }

    /**
     * Decode guest byte payloads that arrive either as a JSON string (raw
     * bytes) or as a JSON array of numbers (see forkd provider byte mapping).
     */
    public static byte[] valueBytes(JsonNode node) {
        if (node == null || node.isNull()) {
            return new byte[0];
        }
        if (node.isTextual()) {
            return node.asText().getBytes(StandardCharsets.UTF_8);
        }
        if (node.isArray()) {
            byte[] out = new byte[node.size()];
            for (int i = 0; i < node.size(); i++) {
                JsonNode v = node.get(i);
                if (!v.isNumber() || v.intValue() < 0 || v.intValue() > 255) {
                    throw new DecodeError("invalid output byte at index " + i);
                }
                out[i] = (byte) v.intValue();
            }
            return out;
        }
        throw new DecodeError("invalid output value: expected string or byte array");
    }

    public static byte[] write(JsonNode node) {
        try {
            return MAPPER.writeValueAsBytes(node);
        } catch (IOException e) {
            throw new DecodeError("cannot encode JSON: " + e.getMessage(), e);
        }
    }

    public static <T> T convert(JsonNode node, Class<T> type) {
        try {
            return MAPPER.treeToValue(node, type);
        } catch (IOException e) {
            throw new DecodeError("invalid JSON payload: " + e.getMessage(), e);
        }
    }

    /** Decode a raw JSON text body (e.g. an HTTP response payload) into {@code type}. */
    public static <T> T convert(String json, Class<T> type) {
        try {
            return MAPPER.readValue(json, type);
        } catch (IOException e) {
            throw new DecodeError("invalid JSON payload: " + e.getMessage(), e);
        }
    }
}
