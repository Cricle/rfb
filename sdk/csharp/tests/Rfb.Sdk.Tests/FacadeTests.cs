using System.Text;
using Xunit;

namespace Rfb.Sdk.Tests;

/// <summary>
/// Public facade tests over BOTH transports (ndjson / zbrt) with identical result
/// shapes — UNIFIED_API.md §9 checklist.
/// </summary>
public class FacadeTests {
    private const string SandboxJson = """
        {{"id":"sb-1","snapshot_tag":"{0}","guest_addr":"{1}","created_at_unix":1700000000,"has_branched":false,"branch_count":0}}
        """;

    private sealed record Fixture(
        RfbClient Client, FakeHttpServer Controller, IDisposable Guest, Sandbox Sandbox)
        : IDisposable {
        public void Dispose() {
            Controller.Dispose();
            Guest.Dispose();
        }
    }

    private static async Task<Fixture> NewFixtureAsync(string transport) {
        IDisposable guest;
        string guestAddr;
        if (transport == "ndjson") {
            var g = new FakeNdjsonGuest();
            guest = g;
            guestAddr = g.Address;
        } else {
            var g = new FakeZbrtServer();
            guest = g;
            guestAddr = g.Address;
        }

        var body = string.Format(System.Globalization.CultureInfo.InvariantCulture, SandboxJson, "base", guestAddr);
        var controller = new FakeHttpServer(req => (req.Method, req.Path) switch {
            ("POST", "/v1/sandboxes") => new FakeHttpServer.Response(200, $"[{body}]"),
            ("GET", "/v1/sandboxes") => new FakeHttpServer.Response(200, $"[{body}]"),
            ("DELETE", "/v1/sandboxes/sb-1") => new FakeHttpServer.Response(404, "gone"),
            ("POST", "/v1/sandboxes/sb-1/ping") => new FakeHttpServer.Response(200, """{"ok":true}"""),
            _ => new FakeHttpServer.Response(404, "not found"),
        });
        var client = new RfbClient(controller.Url, token: null);
        var sandboxes = await client.CreateSandbox("base", transport: transport);
        return new Fixture(client, controller, guest, sandboxes[0]);
    }

    [Theory]
    [InlineData("ndjson")]
    [InlineData("zbrt")]
    public async Task Exec_IdenticalShapes(string transport) {
        using var fx = await NewFixtureAsync(transport);
        var result = await fx.Sandbox.Exec(new[] { "echo", "hi" });
        Assert.Equal(0, result.ExitCode);
        Assert.Equal("hi\n"u8.ToArray(), result.Stdout);
        Assert.Equal("e\n"u8.ToArray(), result.Stderr);
        Assert.Equal("hi\n", result.StdoutText);
        Assert.Equal("e\n", result.StderrText);
        Assert.False(result.TimedOut);
    }

    [Fact]
    public async Task Exec_DropsStdinOverNdjson() {
        using var fx = await NewFixtureAsync("ndjson");
        // Rust baseline: the NDJSON exec wire contract has no stdin channel;
        // non-empty stdin is silently dropped (delivered only over ZBRT).
        var result = await fx.Sandbox.Exec(new[] { "echo", "hi" }, stdin: new byte[] { 1, 2, 3 });
        Assert.Equal(0, result.ExitCode);
    }

    [Theory]
    [InlineData("ndjson")]
    [InlineData("zbrt")]
    public async Task Eval_OutputMapsToStdout_IdenticalShapes(string transport) {
        using var fx = await NewFixtureAsync(transport);
        var result = await fx.Sandbox.Eval("1+1", cwd: "/workspace", timeoutS: 5);
        Assert.Equal(0, result.ExitCode);
        Assert.Equal("2"u8.ToArray(), result.Stdout);
        Assert.Equal("2", result.StdoutText);
        Assert.Empty(result.Stderr);
        Assert.False(result.TimedOut);
    }

    [Theory]
    [InlineData("ndjson")]
    [InlineData("zbrt")]
    public async Task Ls_IdenticalShapes(string transport) {
        using var fx = await NewFixtureAsync(transport);
        var entries = await fx.Sandbox.Ls();
        Assert.Equal(2, entries.Count);
        Assert.Equal("a.txt", entries[0].Name);
        Assert.False(entries[0].IsDir);
        Assert.Equal(3, entries[0].Size);
        Assert.Equal("sub", entries[1].Name);
        Assert.True(entries[1].IsDir);
        Assert.Null(entries[1].Size);
    }

    [Theory]
    [InlineData("ndjson")]
    [InlineData("zbrt")]
    public async Task Find_IdenticalShapes(string transport) {
        using var fx = await NewFixtureAsync(transport);
        var matches = await fx.Sandbox.Find(path: "/workspace", pattern: "*.txt");
        Assert.Equal(new[] { "a.txt" }, matches);
    }

    [Theory]
    [InlineData("ndjson")]
    [InlineData("zbrt")]
    public async Task Grep_IdenticalShapes(string transport) {
        using var fx = await NewFixtureAsync(transport);
        var matches = await fx.Sandbox.Grep(path: ".", pattern: "hi");
        Assert.Equal(2, matches.Count);
        Assert.Equal("a.txt", matches[0].Path);
        Assert.Equal(1, matches[0].Line);
        Assert.Equal(1, matches[0].Column);
        Assert.Equal("hi", matches[0].Text);
        Assert.Equal("b.txt", matches[1].Path);
        Assert.Null(matches[1].Line);
        Assert.Null(matches[1].Column);
        Assert.Equal("hm", matches[1].Text);
    }

    [Theory]
    [InlineData("ndjson")]
    [InlineData("zbrt")]
    public async Task Read_IdenticalShapes(string transport) {
        using var fx = await NewFixtureAsync(transport);
        var read = await fx.Sandbox.Read("/workspace/a.txt");
        Assert.Equal("hi"u8.ToArray(), read.Data);
        Assert.False(read.Truncated);
        Assert.Null(read.TotalBytes);
    }

    [Fact]
    public async Task Read_WithOffsetAndLimit_Ndjson() {
        using var fx = await NewFixtureAsync("ndjson");
        var read = await fx.Sandbox.Read("a.txt", offset: 10, maxBytes: 1024);
        Assert.Equal("hi"u8.ToArray(), read.Data);
    }

    [Theory]
    [InlineData("ndjson")]
    [InlineData("zbrt")]
    public async Task Write_IdenticalShapes(string transport) {
        using var fx = await NewFixtureAsync(transport);
        var written = await fx.Sandbox.Write("/workspace/a.txt", "hello"u8.ToArray(), append: true);
        Assert.Equal(2, written);
    }

    [Theory]
    [InlineData("ndjson")]
    [InlineData("zbrt")]
    public async Task Ping_IdenticalShapes(string transport) {
        using var fx = await NewFixtureAsync(transport);
        Assert.True(await fx.Sandbox.Ping());
    }

    [Theory]
    [InlineData("ndjson")]
    [InlineData("zbrt")]
    public async Task Delete_SucceedsOn404(string transport) {
        using var fx = await NewFixtureAsync(transport);
        await fx.Sandbox.Delete(); // controller answers 404 → success
        await fx.Client.DeleteSandbox("sb-1");
    }

    [Fact]
    public async Task PingSandbox_ReturnsRawJsonValue() {
        using var fx = await NewFixtureAsync("ndjson");
        var value = await fx.Client.PingSandbox("sb-1");
        Assert.True(value.GetProperty("ok").GetBoolean());
    }

    [Fact]
    public async Task Connect_BySandbox_And_ById() {
        using var fx = await NewFixtureAsync("ndjson");
        var bySandbox = await fx.Client.Connect(fx.Sandbox);
        Assert.Equal("sb-1", bySandbox.Id);
        Assert.Equal(fx.Sandbox.GuestAddr, bySandbox.GuestAddr);

        var byId = await fx.Client.Connect("sb-1");
        Assert.Equal("sb-1", byId.Id);
    }

    [Fact]
    public async Task Connect_UnknownId_RaisesRemoteError() {
        using var fx = await NewFixtureAsync("ndjson");
        await Assert.ThrowsAsync<RemoteException>(() => fx.Client.Connect("ghost"));
    }

    [Fact]
    public async Task Connect_BadId_RaisesValidationError() {
        using var fx = await NewFixtureAsync("ndjson");
        await Assert.ThrowsAsync<ValidationException>(() => fx.Client.Connect("bad id!"));
    }

    // ---- error classes -----------------------------------------------------

    [Theory]
    [InlineData("ndjson")]
    [InlineData("zbrt")]
    public async Task RemoteError_MappedOnBothTransports(string transport) {
        using var fx = await NewFixtureAsync(transport);
        var e = await Assert.ThrowsAsync<RemoteException>(
            () => fx.Sandbox.Exec(new[] { "fail" }));
        Assert.Contains("boom", e.Message);
    }

    [Theory]
    [InlineData("ndjson")]
    [InlineData("zbrt")]
    public async Task Validation_FailsClosed_BeforeSending(string transport) {
        using var fx = await NewFixtureAsync(transport);
        await Assert.ThrowsAsync<ValidationException>(
            () => fx.Sandbox.Exec(Array.Empty<string>()));
        await Assert.ThrowsAsync<ValidationException>(
            () => fx.Sandbox.Exec(new[] { "echo" }, cwd: "../escape"));
        await Assert.ThrowsAsync<ValidationException>(
            () => fx.Sandbox.Exec(new[] { "echo" }, timeoutS: 0));
        await Assert.ThrowsAsync<ValidationException>(
            () => fx.Sandbox.Exec(new[] { "echo" }, timeoutS: -1));
        await Assert.ThrowsAsync<ValidationException>(
            () => fx.Sandbox.Exec(new[] { "echo" }, timeoutS: double.PositiveInfinity));
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Read("../etc/passwd"));
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Ls("/etc"));
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Ls("a\\b"));
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Grep(new string('p', 2000)));
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Grep(""));
        await Assert.ThrowsAsync<ValidationException>(
            () => fx.Sandbox.Write("f", new byte[GuestValidation_MaxResultBytes + 1]));
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Eval("   "));
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Eval("1", timeoutS: 0));
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Eval("1", timeoutS: -0.5));
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Eval("1", timeoutS: double.NaN));
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Eval("1", cwd: "../escape"));
        // timeout 0.2 is valid (non-zero) — exercised by Eval tests above
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Stream(Array.Empty<string>()));
        await Assert.ThrowsAsync<ValidationException>(
            () => fx.Sandbox.Read("f", maxBytes: GuestValidation_MaxResultBytes + 1));
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Read("f", maxBytes: 0));
    }

    private const long GuestValidation_MaxResultBytes = 51200;

    [Fact]
    public async Task TransportError_WhenGuestUnreachable() {
        // controller returns a guest_addr pointing at a closed port
        var body = """{"id":"sb-1","snapshot_tag":"base","guest_addr":"127.0.0.1:1","has_branched":false,"branch_count":0}""";
        using var controller = new FakeHttpServer(_ => new FakeHttpServer.Response(200, $"[{body}]"));
        var client = new RfbClient(controller.Url, token: null);
        var sandbox = (await client.CreateSandbox("base"))[0];
        await Assert.ThrowsAsync<TransportException>(() => sandbox.Exec(new[] { "echo" }));
        await Assert.ThrowsAsync<TransportException>(() => sandbox.Ping());
    }

    [Fact]
    public async Task HttpStatusError_MapsThroughFacade() {
        using var controller = new FakeHttpServer(_ => new FakeHttpServer.Response(500, """{"error":"kaputt"}"""));
        var client = new RfbClient(controller.Url, token: null);
        var e = await Assert.ThrowsAsync<HttpStatusException>(() => client.ListSnapshots());
        Assert.Equal(500, e.Status);
    }

    [Fact]
    public async Task DecodeError_MapsThroughFacade() {
        using var controller = new FakeHttpServer(_ => new FakeHttpServer.Response(200, "not-json"));
        var client = new RfbClient(controller.Url, token: null);
        await Assert.ThrowsAsync<DecodeException>(() => client.ListSnapshots());
    }

    // ---- controller-level facade behavior -----------------------------------

    [Fact]
    public async Task CreateSandbox_SendsCreateSandboxRequestBody() {
        string? captured = null;
        var body = """{"id":"sb-1","snapshot_tag":"base","guest_addr":"127.0.0.1:1","has_branched":false,"branch_count":0}""";
        using var controller = new FakeHttpServer(req => {
            if (req.Path == "/v1/sandboxes" && req.Method == "POST") {
                captured = req.Body;
            }

            return new FakeHttpServer.Response(200, $"[{body}]");
        });
        var client = new RfbClient(controller.Url, token: null);
        await client.CreateSandbox("base", n: 3, perChildNetns: true, memoryLimitMib: 512, prewarm: true, liveFork: true, hugepages: true);
        Assert.NotNull(captured);
        using var doc = System.Text.Json.JsonDocument.Parse(captured!);
        var root = doc.RootElement;
        Assert.Equal("base", root.GetProperty("snapshot_tag").GetString());
        Assert.Equal(3, root.GetProperty("n").GetInt32());
        Assert.True(root.GetProperty("per_child_netns").GetBoolean());
        Assert.Equal(512, root.GetProperty("memory_limit_mib").GetInt32());
        Assert.True(root.GetProperty("prewarm").GetBoolean());
        Assert.True(root.GetProperty("live_fork").GetBoolean());
        Assert.True(root.GetProperty("hugepages").GetBoolean());
    }

    [Fact]
    public async Task WaitSnapshot_ReturnsWhenReady() {
        using var controller = new FakeHttpServer(_ => new FakeHttpServer.Response(
            200, """[{"tag":"s1","status":"ready","bootable":true}]"""));
        var client = new RfbClient(controller.Url, token: null);
        var snapshot = await client.WaitSnapshot("s1", timeoutS: 2);
        Assert.Equal("s1", snapshot.Tag);
        Assert.True(snapshot.Bootable);
    }

    [Fact]
    public async Task WaitSnapshot_Failed_RaisesRemoteError() {
        using var controller = new FakeHttpServer(_ => new FakeHttpServer.Response(
            200, """[{"tag":"s1","status":"failed","bootable":false}]"""));
        var client = new RfbClient(controller.Url, token: null);
        await Assert.ThrowsAsync<RemoteException>(() => client.WaitSnapshot("s1", timeoutS: 2));
    }

    [Fact]
    public async Task WaitSnapshot_Timeout_RaisesTransportError() {
        using var controller = new FakeHttpServer(_ => new FakeHttpServer.Response(
            200, """[{"tag":"s1","status":"building","bootable":false}]"""));
        var client = new RfbClient(controller.Url, token: null);
        await Assert.ThrowsAsync<TransportException>(() => client.WaitSnapshot("s1", timeoutS: 0.3));
    }

    [Fact]
    public async Task Snapshot_Missing_ReturnsNull() {
        using var controller = new FakeHttpServer(_ => new FakeHttpServer.Response(404, "missing"));
        var client = new RfbClient(controller.Url, token: null);
        Assert.Null(await client.Snapshot("ghost"));
    }

    // ---- streams -------------------------------------------------------------

    [Fact]
    public async Task Stream_Ndjson_EventsInputStop() {
        using var fx = await NewFixtureAsync("ndjson");
        var stream = await fx.Sandbox.Stream(new[] { "cat" }, cwd: "/workspace", pty: true);
        var started = await stream.NextEvent();
        Assert.Equal(StreamEventKind.Started, started!.Kind);

        await stream.SendInput("hello");
        var echoed = await stream.NextEvent();
        Assert.Equal(StreamEventKind.Stdout, echoed!.Kind);
        Assert.Equal("echo:hello"u8.ToArray(), echoed.Data);

        await stream.Stop();
        var exit = await stream.NextEvent();
        Assert.Equal(StreamEventKind.Exit, exit!.Kind);
        Assert.Equal(9, exit.Code);
        Assert.Null(await stream.NextEvent());
        await Assert.ThrowsAsync<RemoteException>(() => stream.SendInput("late"));
    }

    [Fact]
    public async Task Stream_Zbrt_EventsStop() {
        using var fx = await NewFixtureAsync("zbrt");
        var stream = await fx.Sandbox.Stream(new[] { "cat" });
        var first = await stream.NextEvent();
        Assert.Equal(StreamEventKind.Stdout, first!.Kind);
        Assert.Equal("hi\n"u8.ToArray(), first.Data);

        await stream.Stop(); // Cancel(reason "stop") → CancelAck skipped
        var exit = await stream.NextEvent(); // terminal Exit delivered after stop
        Assert.Equal(StreamEventKind.Exit, exit!.Kind);
        Assert.Equal(-1, exit.Code);
        Assert.Null(await stream.NextEvent()); // sequence ended
        await Assert.ThrowsAsync<RemoteException>(() => stream.SendInput("late"));
    }

    [Fact]
    public async Task Stream_Zbrt_RejectsEnvAndPty() {
        using var fx = await NewFixtureAsync("zbrt");
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Stream(
            new[] { "cat" }, env: new Dictionary<string, string> { ["A"] = "1" }));
        await Assert.ThrowsAsync<ValidationException>(() => fx.Sandbox.Stream(
            new[] { "cat" }, pty: true));
    }

    [Fact]
    public async Task Zbrt_Fs_Frames_CarryNullKeysPerProtocol() {
        // PROTOCOL.md §3.3: read = {"offset":null,"max_bytes":null};
        // write = {"data":[...],"append":false,"mode":null}
        using var fx = await NewFixtureAsync("zbrt");
        var server = Assert.IsType<FakeZbrtServer>(fx.Guest);

        await fx.Sandbox.Read("a.txt");
        Assert.Equal(4, server.LastFsOp);
        Assert.Equal("""{"offset":null,"max_bytes":null}""", server.LastFsJson);

        await fx.Sandbox.Write("/workspace/a.txt", "hi"u8.ToArray(), append: false);
        Assert.Equal(5, server.LastFsOp);
        Assert.Equal("""{"data":[104,105],"append":false,"mode":null}""", server.LastFsJson);
    }

    [Fact]
    public async Task Sandbox_Properties_MirrorSandboxInfo() {
        using var fx = await NewFixtureAsync("ndjson");
        Assert.Equal("sb-1", fx.Sandbox.Id);
        Assert.Equal("base", fx.Sandbox.SnapshotTag);
        Assert.Equal(1700000000, fx.Sandbox.CreatedAtUnix);
        Assert.NotNull(fx.Sandbox.Info);
        Assert.Equal("ndjson", fx.Sandbox.Transport);
    }
}
