package io.rfb.sdk.internal;

import com.fasterxml.jackson.databind.JsonNode;
import io.rfb.sdk.HttpStatusError;
import io.rfb.sdk.SandboxInfo;
import io.rfb.sdk.Snapshot;
import io.rfb.sdk.TransportError;
import io.rfb.sdk.ValidationError;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.HttpURLConnection;
import java.net.URI;
import java.net.URL;
import java.net.URLEncoder;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * forkd controller HTTP/JSON transport (PROTOCOL.md §1). Built on
 * {@code HttpURLConnection} so the artifact supports Java 8. INTERNAL — the
 * public surface is {@code io.rfb.sdk.RfbClient}.
 */
public final class ControllerHttp {
    public static final String DEFAULT_URL = "http://127.0.0.1:8889";

    private final String baseUrl;
    private final String token;
    private final Duration timeout;

    public ControllerHttp(String baseUrl, String token, Duration timeout) {
        if (timeout == null || timeout.isNegative() || timeout.isZero()) {
            throw new ValidationError("forkd timeout must be positive");
        }
        parseBaseUrl(baseUrl);
        this.baseUrl = baseUrl.replaceAll("/+$", "");
        this.token = token == null || token.trim().isEmpty() ? null : token;
        this.timeout = timeout;
    }

    public static String envUrl() {
        String url = System.getenv("FORKD_URL");
        return url == null || url.isEmpty() ? DEFAULT_URL : url;
    }

    /** Non-blank {@code FORKD_TOKEN} value, else null. */
    public static String envToken() {
        String token = System.getenv("FORKD_TOKEN");
        return token == null || token.trim().isEmpty() ? null : token;
    }

    private static void parseBaseUrl(String baseUrl) {
        URI uri;
        try {
            uri = URI.create(baseUrl);
        } catch (IllegalArgumentException e) {
            throw new ValidationError("invalid forkd URL: " + e.getMessage());
        }
        String scheme = uri.getScheme() == null ? "" : uri.getScheme().toLowerCase();
        if (!(scheme.equals("http") || scheme.equals("https")) || uri.getHost() == null) {
            throw new ValidationError("forkd URL must include http(s) scheme and host");
        }
    }

    public List<Snapshot> listSnapshots() {
        return getArray("/v1/snapshots", Snapshot[].class);
    }

    /**
     * Snapshot detail: preferred {@code /v1/snapshots/{tag}/info}, 404 falls
     * back to legacy {@code /v1/snapshots/{tag}}; both 404 → null.
     */
    public Snapshot snapshotInfo(String tag) {
        HttpResult preferred = send("GET", "/v1/snapshots/" + urlSegment(tag) + "/info", null, null);
        if (preferred.statusCode != 404) {
            return Json.convert(expectOk(preferred), Snapshot.class);
        }
        HttpResult legacy = send("GET", "/v1/snapshots/" + urlSegment(tag), null, null);
        if (legacy.statusCode == 404) {
            return null;
        }
        return Json.convert(expectOk(legacy), Snapshot.class);
    }

    public List<SandboxInfo> createSandbox(String snapshotTag, int n, boolean perChildNetns,
                                           Long memoryLimitMib, boolean prewarm, boolean liveFork,
                                           boolean hugepages) {
        JsonNode body = Json.object()
                .put("snapshot_tag", snapshotTag)
                .put("n", n)
                .put("per_child_netns", perChildNetns)
                .put("memory_limit_mib", memoryLimitMib)
                .put("prewarm", prewarm)
                .put("live_fork", liveFork)
                .put("hugepages", hugepages);
        HttpResult resp = send("POST", "/v1/sandboxes", Json.write(body), "application/json");
        SandboxInfo[] arr = Json.convert(expectOk(resp), SandboxInfo[].class);
        return new ArrayList<>(Arrays.asList(arr));
    }

    public List<SandboxInfo> listSandboxes() {
        return getArray("/v1/sandboxes", SandboxInfo[].class);
    }

    /** Ping a sandbox; returns the controller's arbitrary JSON response value. */
    public JsonNode pingSandbox(String sandboxId) {
        Validation.sandboxId(sandboxId);
        HttpResult resp = send("POST", "/v1/sandboxes/" + urlSegment(sandboxId) + "/ping", null, null);
        return Json.parse(expectOk(resp).getBytes(StandardCharsets.UTF_8));
    }

    /** Delete a sandbox; both 2xx and 404 are success. */
    public void deleteSandbox(String sandboxId) {
        Validation.sandboxId(sandboxId);
        HttpResult resp = send("DELETE", "/v1/sandboxes/" + urlSegment(sandboxId), null, null);
        int status = resp.statusCode;
        if (status == 404 || (status >= 200 && status <= 299)) {
            return;
        }
        throw httpError(status, resp.body);
    }

    // ---- plumbing --------------------------------------------------------

    /** GET an endpoint returning a JSON array of {@code type} elements. */
    private <T> List<T> getArray(String path, Class<T[]> type) {
        T[] arr = Json.convert(expectOk(send("GET", path, null, null)), type);
        return new ArrayList<>(Arrays.asList(arr));
    }

    private HttpResult send(String method, String pathAndQuery, byte[] body, String contentType) {
        HttpURLConnection conn = null;
        try {
            URL url = new URL(baseUrl + pathAndQuery);
            conn = (HttpURLConnection) url.openConnection();
            int timeoutMs = (int) Math.min(Integer.MAX_VALUE, timeout.toMillis());
            conn.setConnectTimeout(timeoutMs);
            conn.setReadTimeout(timeoutMs);
            conn.setRequestMethod(method);
            if (token != null) {
                conn.setRequestProperty("Authorization", "Bearer " + token);
            }
            if (body != null) {
                conn.setDoOutput(true);
                if (contentType != null) {
                    conn.setRequestProperty("Content-Type", contentType);
                }
            }
            if (body != null) {
                java.io.OutputStream out = conn.getOutputStream();
                try {
                    out.write(body);
                } finally {
                    out.close();
                }
            }
            int status = conn.getResponseCode();
            try (InputStream stream =
                    status >= 400 ? conn.getErrorStream() : conn.getInputStream()) {
                return new HttpResult(status, readAll(stream));
            }
        } catch (IOException e) {
            throw new TransportError("forkd request failed: " + e.getMessage(), e);
        } finally {
            if (conn != null) {
                conn.disconnect();
            }
        }
    }

    private static String readAll(InputStream in) throws IOException {
        if (in == null) {
            return "";
        }
        ByteArrayOutputStream buffer = new ByteArrayOutputStream();
        byte[] chunk = new byte[8192];
        int n;
        while ((n = in.read(chunk)) > 0) {
            buffer.write(chunk, 0, n);
        }
        return new String(buffer.toByteArray(), StandardCharsets.UTF_8);
    }

    private String expectOk(HttpResult resp) {
        int status = resp.statusCode;
        if (status < 200 || status > 299) {
            throw httpError(status, resp.body);
        }
        return resp.body;
    }

    private HttpStatusError httpError(int status, String body) {
        String message = null;
        if (body != null && !body.isEmpty()) {
            try {
                JsonNode node = Json.MAPPER.readTree(body);
                if (node != null && node.has("error") && node.get("error").isTextual()) {
                    message = node.get("error").asText();
                }
            } catch (IOException ignored) {
                // fall through to raw body
            }
            if (message == null) {
                message = body.substring(0, Math.min(1024, body.length()));
            }
        } else {
            message = "";
        }
        return new HttpStatusError(status, message);
    }

    private static String urlSegment(String value) {
        try {
            return URLEncoder.encode(value, "UTF-8").replace("+", "%20");
        } catch (java.io.UnsupportedEncodingException e) {
            throw new TransportError("UTF-8 encoding missing", e);
        }
    }

    /** Minimal response value holder (replaces java.net.http.HttpResponse). */
    private static final class HttpResult {
        final int statusCode;
        final String body;

        HttpResult(int statusCode, String body) {
            this.statusCode = statusCode;
            this.body = body;
        }
    }
}
