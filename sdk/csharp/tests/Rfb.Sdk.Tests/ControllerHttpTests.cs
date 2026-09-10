using System.Text.Json;
using Rfb.Sdk.Internal;
using Xunit;

namespace Rfb.Sdk.Tests;

/// <summary>forkd controller HTTP adapter against a fake HTTP server (PROTOCOL.md §1).</summary>
public class ControllerHttpTests
{
    private static string SnapshotJson(string tag, string status = "ready", bool bootable = true) =>
        System.Text.Json.JsonSerializer.Serialize(new Dictionary<string, object?> {
            ["tag"] = tag,
            ["dir"] = $"/s/{tag}",
            ["created_at_unix"] = 123,
            ["status"] = status,
            ["bootable"] = bootable,
            ["provenance"] = new Dictionary<string, object?> { ["k"] = 1 },
        });

    private static string SandboxJson(string id, string addr) =>
        System.Text.Json.JsonSerializer.Serialize(new Dictionary<string, object?> {
            ["id"] = id,
            ["snapshot_tag"] = "base",
            ["guest_addr"] = addr,
            ["has_branched"] = false,
            ["branch_count"] = 0,
        });

    [Fact]
    public async Task ListSnapshots_ParsesArray()
    {
        using var server = new FakeHttpServer(_ =>
            new FakeHttpServer.Response(200, $"[{SnapshotJson("a")},{SnapshotJson("b")}]"));
        var http = new ForkdControllerHttp(server.Url, null, TimeSpan.FromSeconds(5));
        var list = await http.ListSnapshotsAsync();
        Assert.Equal(2, list.GetArrayLength());
        Assert.Equal("a", list[0].GetProperty("tag").GetString());
    }

    [Fact]
    public async Task SnapshotInfo_UsesPreferredEndpoint()
    {
        using var server = new FakeHttpServer(req => req.Path switch
        {
            "/v1/snapshots/base/info" => new FakeHttpServer.Response(200, SnapshotJson("base")),
            _ => new FakeHttpServer.Response(500, "{}"),
        });
        var http = new ForkdControllerHttp(server.Url, null, TimeSpan.FromSeconds(5));
        var info = await http.SnapshotInfoAsync("base");
        Assert.NotNull(info);
        Assert.Equal("base", info.Value.GetProperty("tag").GetString());
    }

    [Fact]
    public async Task SnapshotInfo_FallsBackToLegacyEndpoint()
    {
        using var server = new FakeHttpServer(req => req.Path switch
        {
            "/v1/snapshots/base/info" => new FakeHttpServer.Response(404, "nope"),
            "/v1/snapshots/base" => new FakeHttpServer.Response(200, SnapshotJson("base")),
            _ => new FakeHttpServer.Response(500, "{}"),
        });
        var http = new ForkdControllerHttp(server.Url, null, TimeSpan.FromSeconds(5));
        var info = await http.SnapshotInfoAsync("base");
        Assert.NotNull(info);
        Assert.Equal("base", info.Value.GetProperty("tag").GetString());
    }

    [Fact]
    public async Task SnapshotInfo_Both404_ReturnsNull()
    {
        using var server = new FakeHttpServer(_ => new FakeHttpServer.Response(404, "missing"));
        var http = new ForkdControllerHttp(server.Url, null, TimeSpan.FromSeconds(5));
        var info = await http.SnapshotInfoAsync("ghost");
        Assert.Null(info);
    }

    [Fact]
    public async Task Non2xx_WithJsonErrorField_MapsMessage()
    {
        using var server = new FakeHttpServer(_ => new FakeHttpServer.Response(500, """{"error":"boom"}"""));
        var http = new ForkdControllerHttp(server.Url, null, TimeSpan.FromSeconds(5));
        var e = await Assert.ThrowsAsync<HttpStatusException>(() => http.ListSnapshotsAsync());
        Assert.Equal(500, e.Status);
        Assert.Equal("boom", e.Message[^4..]);
    }

    [Fact]
    public async Task Non2xx_WithPlainBody_TakesFirst1024Chars()
    {
        var body = new string('x', 2000);
        using var server = new FakeHttpServer(_ => new FakeHttpServer.Response(502, body));
        var http = new ForkdControllerHttp(server.Url, null, TimeSpan.FromSeconds(5));
        var e = await Assert.ThrowsAsync<HttpStatusException>(() => http.ListSnapshotsAsync());
        Assert.Equal(502, e.Status);
        Assert.Equal(1024, e.Message.Length - "forkd returned 502: ".Length);
    }

    [Fact]
    public async Task BearerToken_SentWhenNonEmpty()
    {
        string? auth = null;
        using var server = new FakeHttpServer(req =>
        {
            auth = req.Headers.GetValueOrDefault("Authorization");
            return new FakeHttpServer.Response(200, "[]");
        });
        var http = new ForkdControllerHttp(server.Url, "sekrit", TimeSpan.FromSeconds(5));
        await http.ListSnapshotsAsync();
        Assert.Equal("Bearer sekrit", auth);
    }

    [Fact]
    public async Task NoToken_NoAuthHeader()
    {
        string? auth = null;
        using var server = new FakeHttpServer(req =>
        {
            req.Headers.TryGetValue("Authorization", out var a);
            auth = a;
            return new FakeHttpServer.Response(200, "[]");
        });
        var http = new ForkdControllerHttp(server.Url, null, TimeSpan.FromSeconds(5));
        await http.ListSnapshotsAsync();
        Assert.Null(auth);
    }

    [Fact]
    public async Task Delete_404_IsSuccess()
    {
        using var server = new FakeHttpServer(_ => new FakeHttpServer.Response(404, "gone"));
        var http = new ForkdControllerHttp(server.Url, null, TimeSpan.FromSeconds(5));
        await http.DeleteSandboxAsync("sb-1");
    }

    [Fact]
    public async Task Delete_2xx_IsSuccess()
    {
        using var server = new FakeHttpServer(_ => new FakeHttpServer.Response(200, "{}"));
        var http = new ForkdControllerHttp(server.Url, null, TimeSpan.FromSeconds(5));
        await http.DeleteSandboxAsync("sb-1");
    }

    [Fact]
    public async Task Delete_500_ThrowsHttpStatus()
    {
        using var server = new FakeHttpServer(_ => new FakeHttpServer.Response(500, """{"error":"nope"}"""));
        var http = new ForkdControllerHttp(server.Url, null, TimeSpan.FromSeconds(5));
        await Assert.ThrowsAsync<HttpStatusException>(() => http.DeleteSandboxAsync("sb-1"));
    }

    [Fact]
    public async Task SandboxId_Validation_FailsClosed()
    {
        var http = new ForkdControllerHttp("http://127.0.0.1:1", null, TimeSpan.FromSeconds(5));
        await Assert.ThrowsAsync<ValidationException>(() => http.PingAsync("bad id!"));
        await Assert.ThrowsAsync<ValidationException>(() => http.PingAsync(new string('a', 129)));
        await Assert.ThrowsAsync<ValidationException>(() => http.DeleteSandboxAsync(""));
    }

    [Fact]
    public async Task ConnectionRefused_MapsToTransportError()
    {
        var http = new ForkdControllerHttp("http://127.0.0.1:1", null, TimeSpan.FromSeconds(5));
        var e = await Assert.ThrowsAsync<TransportException>(() => http.ListSnapshotsAsync());
        Assert.Contains("forkd request failed", e.Message);
    }

    [Fact]
    public async Task InvalidBody_MapsToDecodeError()
    {
        using var server = new FakeHttpServer(_ => new FakeHttpServer.Response(200, "not json"));
        var http = new ForkdControllerHttp(server.Url, null, TimeSpan.FromSeconds(5));
        await Assert.ThrowsAsync<DecodeException>(() => http.ListSnapshotsAsync());
    }

    [Fact]
    public void InvalidBaseUrl_ThrowsValidation()
    {
        Assert.Throws<ValidationException>(() =>
            new ForkdControllerHttp("ftp://nope", null, TimeSpan.FromSeconds(5)));
    }
}
