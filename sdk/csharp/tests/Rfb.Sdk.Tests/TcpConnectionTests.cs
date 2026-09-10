using System.Text.Json;
using Xunit;

namespace Rfb.Sdk.Tests;

/// <summary>
/// TCP connection-count regression tests: the controller HTTP client must reuse
/// one pooled keep-alive connection per RfbClient, while the NDJSON guest keeps
/// its connection-per-request semantics (Rust reference behavior).
/// </summary>
public class TcpConnectionTests
{
    private static string SnapshotsJson() =>
        "[" + JsonSerializer.Serialize(new Dictionary<string, object?> {
            ["tag"] = "a", ["dir"] = "/s/a", ["status"] = "ready", ["bootable"] = true,
        }) + "]";

    private static string SandboxJson(string id, string addr) =>
        JsonSerializer.Serialize(new Dictionary<string, object?> {
            ["id"] = id, ["snapshot_tag"] = "base", ["guest_addr"] = addr,
            ["has_branched"] = false, ["branch_count"] = 0,
        });

    [Fact]
    public async Task Controller_N_Requests_Use_One_Accept()
    {
        using var server = new FakeKeepAliveHttpServer(_ => new FakeHttpServer.Response(200, SnapshotsJson()));
        var client = new RfbClient(server.Url);
        const int requests = 10;
        for (var i = 0; i < requests; i++)
        {
            var list = await client.ListSnapshots();
            Assert.Single(list);
        }

        Assert.Equal(1, server.AcceptCount);
    }

    [Fact]
    public async Task Controller_Sandbox_Lifecycle_Uses_One_Accept()
    {
        using var guest = new FakeNdjsonGuest();
        using var server = new FakeKeepAliveHttpServer(req => req.Path switch
        {
            "/v1/sandboxes" => new FakeHttpServer.Response(200, $"[{SandboxJson("sb-1", guest.Address)}]"),
            "/v1/sandboxes/sb-1/ping" => new FakeHttpServer.Response(200, """{"pong":true}"""),
            _ => new FakeHttpServer.Response(404, "nope"),
        });
        var client = new RfbClient(server.Url);

        // Create + connect + ping all ride the one pooled controller connection.
        var sandboxes = await client.CreateSandbox("base");
        Assert.Single(sandboxes);
        var sandbox = await client.Connect("sb-1");
        Assert.True(await sandbox.Ping());
        await client.PingSandbox("sb-1");

        Assert.Equal(1, server.AcceptCount);
        // The guest saw exactly one connection for the sandbox's single ping.
        Assert.Equal(1, guest.AcceptCount);
    }

    [Fact]
    public async Task Guest_Ndjson_Keeps_Connection_Per_Request()
    {
        using var guest = new FakeNdjsonGuest();
        using var server = new FakeKeepAliveHttpServer(_ =>
            new FakeHttpServer.Response(200, $"[{SandboxJson("sb-1", guest.Address)}]"));
        var client = new RfbClient(server.Url);
        var sandbox = (await client.CreateSandbox("base"))[0];

        const int execs = 3;
        for (var i = 0; i < execs; i++)
        {
            var result = await sandbox.Exec(new[] { "echo", "hi" });
            Assert.Equal(0, result.ExitCode);
        }

        // NDJSON guest: connection-per-request (invariable reference semantics).
        Assert.Equal(execs, guest.AcceptCount);
    }
}
