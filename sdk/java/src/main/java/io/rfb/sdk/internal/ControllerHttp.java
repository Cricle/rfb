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

    /** Methods safe to replay once after a stale-connection failure. */
    private static final java.util.Set<String> IDEMPOTENT_METHODS =
            new java.util.HashSet<>(Arrays.asList("GET", "HEAD", "DELETE"));

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

    /**
     * {@code FORKD_URL} value, or the default when the variable is unset OR
     * blank (UNIFIED_API.md §8: unset and whitespace-only both fall back).
     */
    public static String envUrl() {
        String url = System.getenv("FORKD_URL");
        return url == null || url.trim().isEmpty() ? DEFAULT_URL : url;
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
        // create 的独立预算：快照恢复可超基础 10s（超时 = 孤儿一个已落地
        // 的沙箱）。
        HttpResult resp = send("POST", "/v1/sandboxes", Json.write(body),
                "application/json", 60_000);
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

    /**
     * Send one request on a pooled keep-alive connection. The
     * {@code HttpURLConnection} is intentionally NOT disconnected after a
     * successful exchange: the JDK returns it to its keep-alive cache so the
     * next sequential request reuses the same TCP connection (one accept for
     * N sequential requests, like the Rust reqwest pool).
     *
     * <p>Idempotent methods (GET/HEAD/DELETE) that fail on a stale pooled
     * connection are retried exactly once on a fresh connection; POST is never
     * replayed (a duplicated create would be a real side effect). Read/connect
     * timeouts are not connection-staleness and are surfaced as-is.
     */
    private HttpResult send(String method, String pathAndQuery, byte[] body, String contentType) {
        return send(method, pathAndQuery, body, contentType, 0);
    }

    /** budgetMsMs > 0 = 本调用的独立预算（create 的快照恢复）；0 = 客户端默认。 */
    private HttpResult send(String method, String pathAndQuery, byte[] body, String contentType,
                            long budgetMs) {
        boolean idempotent = IDEMPOTENT_METHODS.contains(method);
        IOException lastFailure = null;
        int attempts = idempotent ? 2 : 1;
        for (int attempt = 0; attempt < attempts; attempt++) {
            HttpURLConnection conn = null;
            try {
                conn = open(method, pathAndQuery, body, contentType, budgetMs);
                int status = conn.getResponseCode();
                try (InputStream stream =
                        status >= 400 ? conn.getErrorStream() : conn.getInputStream()) {
                    return new HttpResult(status, readAll(stream));
                }
            } catch (IOException e) {
                lastFailure = e;
                if (conn != null) {
                    // A failed exchange must never go back into the pool.
                    conn.disconnect();
                }
                if (!idempotent || e instanceof java.net.SocketTimeoutException) {
                    break;
                }
                // Stale keep-alive connection: retry once on a fresh one.
            }
        }
        throw new TransportError("forkd request failed: " + lastFailure.getMessage(), lastFailure);
    }

    /** Open and write one request; the connection stays pooled on success. */
    private HttpURLConnection open(String method, String pathAndQuery, byte[] body, String contentType,
                                   long budgetMs)
            throws IOException {
        URL url = new URL(baseUrl + pathAndQuery);
        HttpURLConnection conn = (HttpURLConnection) url.openConnection();
        int timeoutMs = (int) Math.min(Integer.MAX_VALUE,
                budgetMs > 0 ? budgetMs : timeout.toMillis());
        conn.setConnectTimeout(timeoutMs);
        conn.setReadTimeout(timeoutMs);
        conn.setRequestMethod(method);
        if (token != null) {
            conn.setRequestProperty("Authorization", "Bearer " + token);
        }
        if (body != null || "POST".equals(method)) {
            // Fixed-length streaming mode writes the body unbuffered AND makes
            // the JDK skip its transparent replay of a POST whose pooled
            // connection died before the status line
            // (sun.net.www.http.HttpClient checks `streaming` before retrying).
            // Without it a dead pooled connection would silently duplicate
            // non-idempotent POSTs.
            conn.setDoOutput(true);
            conn.setFixedLengthStreamingMode(body == null ? 0 : body.length);
            if (contentType != null) {
                conn.setRequestProperty("Content-Type", contentType);
            }
            java.io.OutputStream out = conn.getOutputStream();
            try {
                if (body != null) {
                    out.write(body);
                }
            } finally {
                out.close();
            }
        }
        return conn;
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
