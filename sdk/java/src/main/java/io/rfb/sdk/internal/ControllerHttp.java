package io.rfb.sdk.internal;

import com.fasterxml.jackson.databind.JsonNode;
import io.rfb.sdk.HttpStatusError;
import io.rfb.sdk.SandboxInfo;
import io.rfb.sdk.Snapshot;
import io.rfb.sdk.TransportError;
import io.rfb.sdk.ValidationError;

import java.io.IOException;
import java.net.URI;
import java.net.URLEncoder;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;

/**
 * forkd controller HTTP/JSON transport (PROTOCOL.md §1). INTERNAL — the public
 * surface is {@code io.rfb.sdk.RfbClient}.
 */
public final class ControllerHttp {
    public static final String DEFAULT_URL = "http://127.0.0.1:8889";

    private final HttpClient http;
    private final String baseUrl;
    private final String token;
    private final Duration timeout;

    public ControllerHttp(String baseUrl, String token, Duration timeout) {
        if (timeout == null || timeout.isNegative() || timeout.isZero()) {
            throw new ValidationError("forkd timeout must be positive");
        }
        parseBaseUrl(baseUrl);
        this.http = HttpClient.newBuilder().connectTimeout(timeout).build();
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
        HttpResponse<String> preferred = send(
                request("GET", "/v1/snapshots/" + urlSegment(tag) + "/info").GET().build());
        if (preferred.statusCode() != 404) {
            return Json.convert(expectOk(preferred), Snapshot.class);
        }
        HttpResponse<String> legacy = send(
                request("GET", "/v1/snapshots/" + urlSegment(tag)).GET().build());
        if (legacy.statusCode() == 404) {
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
        HttpResponse<String> resp = send(request("POST", "/v1/sandboxes")
                .header("Content-Type", "application/json")
                .POST(HttpRequest.BodyPublishers.ofByteArray(Json.write(body)))
                .build());
        SandboxInfo[] arr = Json.convert(expectOk(resp), SandboxInfo[].class);
        return new ArrayList<>(List.of(arr));
    }

    public List<SandboxInfo> listSandboxes() {
        return getArray("/v1/sandboxes", SandboxInfo[].class);
    }

    /** Ping a sandbox; returns the controller's arbitrary JSON response value. */
    public JsonNode pingSandbox(String sandboxId) {
        Validation.sandboxId(sandboxId);
        HttpResponse<String> resp = send(request("POST", "/v1/sandboxes/" + urlSegment(sandboxId) + "/ping")
                .POST(HttpRequest.BodyPublishers.noBody())
                .build());
        return Json.parse(expectOk(resp).getBytes(StandardCharsets.UTF_8));
    }

    /** Delete a sandbox; both 2xx and 404 are success. */
    public void deleteSandbox(String sandboxId) {
        Validation.sandboxId(sandboxId);
        HttpResponse<String> resp = send(request("DELETE", "/v1/sandboxes/" + urlSegment(sandboxId))
                .DELETE()
                .build());
        int status = resp.statusCode();
        if (status == 404 || (status >= 200 && status <= 299)) {
            return;
        }
        throw httpError(status, resp.body());
    }

    // ---- plumbing --------------------------------------------------------

    /** GET an endpoint returning a JSON array of {@code type} elements. */
    private <T> List<T> getArray(String path, Class<T[]> type) {
        T[] arr = Json.convert(expectOk(send(request("GET", path).GET().build())), type);
        return new ArrayList<>(List.of(arr));
    }

    private HttpRequest.Builder request(String method, String pathAndQuery) {
        HttpRequest.Builder b = HttpRequest.newBuilder(URI.create(baseUrl + pathAndQuery))
                .timeout(timeout);
        if (token != null) {
            b.header("Authorization", "Bearer " + token);
        }
        return b;
    }

    private HttpResponse<String> send(HttpRequest req) {
        try {
            return http.send(req, HttpResponse.BodyHandlers.ofString());
        } catch (IOException e) {
            throw new TransportError("forkd request failed: " + e.getMessage(), e);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new TransportError("forkd request interrupted", e);
        }
    }

    private String expectOk(HttpResponse<String> resp) {
        int status = resp.statusCode();
        if (status < 200 || status > 299) {
            throw httpError(status, resp.body());
        }
        return resp.body();
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
        return URLEncoder.encode(value, StandardCharsets.UTF_8).replace("+", "%20");
    }
}
