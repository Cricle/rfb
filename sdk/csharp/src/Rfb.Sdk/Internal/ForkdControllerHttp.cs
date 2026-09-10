using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Rfb.Sdk.Internal;

/// <summary>
/// One raw controller response: status line + body text.
/// </summary>
internal readonly record struct RawResponse(int Status, string Body)
{
    public bool IsSuccess => Status is >= 200 and < 300;
}

/// <summary>
/// HTTP/JSON adapter for the forkd controller (PROTOCOL.md §1; mirror of
/// rfb/src/forkd/controller.rs). Internal only — never part of the public API.
/// </summary>
internal sealed class ForkdControllerHttp
{
    public const string DefaultBaseUrl = "http://127.0.0.1:8889";

    private readonly HttpClient _http;
    private readonly string _baseUrl;
    private readonly string? _token;

    public ForkdControllerHttp(string baseUrl, string? token, TimeSpan timeout, HttpMessageHandler? handler = null)
    {
        var url = baseUrl.TrimEnd('/');
        if (!Uri.TryCreate(url, UriKind.Absolute, out var uri)
            || (uri.Scheme != "http" && uri.Scheme != "https")
            || uri.HostNameType != UriHostNameType.Dns && uri.HostNameType != UriHostNameType.IPv4 && uri.HostNameType != UriHostNameType.IPv6)
        {
            throw new ValidationException("forkd URL must include http(s) scheme and host");
        }

        _baseUrl = url;
        _token = string.IsNullOrWhiteSpace(token) ? null : token;
        _http = handler is null ? new HttpClient() : new HttpClient(handler, disposeHandler: false);
        _http.Timeout = timeout;
    }

    public async Task<JsonElement> ListSnapshotsAsync() =>
        await SendForJsonAsync(HttpMethod.Get, "/v1/snapshots", null);

    /// <summary>/info → legacy endpoint fallback; both 404 → null.</summary>
    public async Task<JsonElement?> SnapshotInfoAsync(string tag)
    {
        var preferred = await RawAsync(HttpMethod.Get, $"/v1/snapshots/{Escape(tag)}/info");
        if (preferred.Status != 404)
        {
            return (await ParseAsync(preferred)).RootElement.Clone();
        }

        var legacy = await RawAsync(HttpMethod.Get, $"/v1/snapshots/{Escape(tag)}");
        if (legacy.Status == 404)
        {
            return null;
        }

        return (await ParseAsync(legacy)).RootElement.Clone();
    }

    public async Task<JsonElement> CreateSandboxesAsync(object body) =>
        await SendForJsonAsync(HttpMethod.Post, "/v1/sandboxes", body);

    public async Task<JsonElement> ListSandboxesAsync() =>
        await SendForJsonAsync(HttpMethod.Get, "/v1/sandboxes", null);

    public async Task<JsonElement> PingAsync(string sandboxId)
    {
        GuestValidation.Id(sandboxId);
        return await SendForJsonAsync(HttpMethod.Post, $"/v1/sandboxes/{Escape(sandboxId)}/ping", null);
    }

    /// <summary>2xx and 404 are both success.</summary>
    public async Task DeleteSandboxAsync(string sandboxId)
    {
        GuestValidation.Id(sandboxId);
        var raw = await RawAsync(HttpMethod.Delete, $"/v1/sandboxes/{Escape(sandboxId)}");
        if (raw.Status == 404 || raw.IsSuccess)
        {
            return;
        }

        await ParseAsync(raw); // throws HttpStatusException with mapped message
    }

    private static string Escape(string segment) => Uri.EscapeDataString(segment);

    private async Task<JsonElement> SendForJsonAsync(HttpMethod method, string path, object? body)
    {
        var raw = await RawAsync(method, path, body);
        return (await ParseAsync(raw)).RootElement.Clone();
    }

    private async Task<RawResponse> RawAsync(HttpMethod method, string path, object? body = null)
    {
        using var request = new HttpRequestMessage(method, _baseUrl + path);
        if (_token is not null)
        {
            request.Headers.Authorization = new System.Net.Http.Headers.AuthenticationHeaderValue("Bearer", _token);
        }

        if (body is not null)
        {
            request.Content = new StringContent(JsonSerializer.Serialize(body), Encoding.UTF8, "application/json");
        }

        HttpResponseMessage response;
        try
        {
            response = await _http.SendAsync(request);
        }
        catch (TaskCanceledException)
        {
            throw new TransportException("forkd request timeout");
        }
        catch (HttpRequestException e)
        {
            throw new TransportException($"forkd request failed: {e.Message}");
        }

        using (response)
        {
            var text = await response.Content.ReadAsStringAsync();
            return new RawResponse((int)response.StatusCode, text);
        }
    }

    /// <summary>Non-2xx → HttpStatusException (body JSON `error` field, else first 1024 chars); else parse JSON.</summary>
    private static async Task<JsonDocument> ParseAsync(RawResponse raw)
    {
        if (!raw.IsSuccess)
        {
            throw new HttpStatusException(raw.Status, ExtractErrorMessage(raw.Body));
        }

        try
        {
            return JsonDocument.Parse(raw.Body);
        }
        catch (JsonException e)
        {
            throw new DecodeException($"invalid forkd response: {e.Message}");
        }
    }

    private static string ExtractErrorMessage(string body)
    {
        try
        {
            using var doc = JsonDocument.Parse(body);
            if (doc.RootElement.ValueKind == JsonValueKind.Object
                && doc.RootElement.TryGetProperty("error", out var error)
                && error.ValueKind == JsonValueKind.String)
            {
                return error.GetString() ?? "";
            }
        }
        catch (JsonException)
        {
            // fall through to raw body
        }

        return body.Length <= 1024 ? body : body[..1024];
    }
}
