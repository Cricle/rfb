using System.Text;
using System.Text.Json;
using Rfb.Sdk;
using Rfb.Sdk.Internal;
using Xunit;

namespace Rfb.Sdk.Tests;

/// <summary>ZBRT TCP client against a fake frame server (PROTOCOL.md §3.4 session semantics).</summary>
public class ZbrtClientTests
{
    private static readonly TimeSpan Timeout = TimeSpan.FromSeconds(5);

    [Fact]
    public async Task Execute_StreamedOutputThenExit()
    {
        using var server = new FakeZbrtServer();
        using var client = new ZbrtTcpClient(server.Address, Timeout);
        await client.ConnectAsync();
        var outcome = await client.ExecuteAsync(new[] { "echo", "hi" }, "/workspace", "abc"u8.ToArray(), 1500);
        Assert.Equal(0, outcome.ExitCode);
        Assert.Equal("hi\n"u8.ToArray(), outcome.Stdout);
        Assert.Equal("e\n"u8.ToArray(), outcome.Stderr);
    }

    [Fact]
    public async Task Execute_ErrorFrame_RaisesRemoteError()
    {
        using var server = new FakeZbrtServer();
        using var client = new ZbrtTcpClient(server.Address, Timeout);
        await client.ConnectAsync();
        var e = await Assert.ThrowsAsync<RemoteException>(
            () => client.ExecuteAsync(new[] { "fail" }, null, [], 0));
        Assert.Equal("boom", e.Message);
    }

    [Fact]
    public async Task Execute_EmptyArgv_ServerFailsClosed()
    {
        using var server = new FakeZbrtServer();
        using var client = new ZbrtTcpClient(server.Address, Timeout);
        await client.ConnectAsync();
        var e = await Assert.ThrowsAsync<RemoteException>(
            () => client.ExecuteAsync(Array.Empty<string>(), null, [], 0));
        Assert.Equal("argv is empty", e.Message);
    }

    [Fact]
    public async Task Hello_YieldsHelloAck()
    {
        using var server = new FakeZbrtServer();
        using var client = new ZbrtTcpClient(server.Address, Timeout);
        await client.ConnectAsync();
        var ack = await client.HelloAsync("sdk-test", new[] { "execute", "stream" });
        Assert.Equal("rfb-zeroboot-guest", ack.Server);
        Assert.Equal(6, ack.Capabilities.Count);
        Assert.Equal("filesystem", ack.Capabilities[^1]);
    }

    [Fact]
    public async Task Health_YieldsHealthAck()
    {
        using var server = new FakeZbrtServer();
        using var client = new ZbrtTcpClient(server.Address, Timeout);
        await client.ConnectAsync();
        Assert.True(await client.HealthAsync());
    }

    [Fact]
    public async Task Fs_LsRoundTrip()
    {
        using var server = new FakeZbrtServer();
        using var client = new ZbrtTcpClient(server.Address, Timeout);
        await client.ConnectAsync();
        var json = JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?> { ["max_results"] = 1000 });
        var result = await client.FsAsync(1, "/workspace", json);
        Assert.Equal("a.txt", result.GetProperty("entries")[0].GetProperty("name").GetString());
    }

    [Fact]
    public async Task Fs_WriteRoundTrip()
    {
        using var server = new FakeZbrtServer();
        using var client = new ZbrtTcpClient(server.Address, Timeout);
        await client.ConnectAsync();
        var json = JsonSerializer.SerializeToUtf8Bytes(new Dictionary<string, object?>
        {
            ["data"] = new byte[] { 1, 2 },
            ["append"] = false,
        });
        var result = await client.FsAsync(5, "/workspace/f.txt", json);
        Assert.Equal(2, result.GetProperty("bytes_written").GetInt32());
    }

    [Fact]
    public async Task Fs_UnknownOp_YieldsErrorFrame()
    {
        using var server = new FakeZbrtServer();
        using var client = new ZbrtTcpClient(server.Address, Timeout);
        await client.ConnectAsync();
        var e = await Assert.ThrowsAsync<RemoteException>(() => client.FsAsync(9, ".", "{}"u8.ToArray()));
        Assert.Contains("unsupported fs op", e.Message);
    }

    [Fact]
    public async Task Stream_Cancel_YieldsCancelAckThenExit()
    {
        using var server = new FakeZbrtServer();
        using var client = new ZbrtTcpClient(server.Address, Timeout);
        await client.ConnectAsync();
        var session = await client.StreamAsync(new[] { "cat" }, null);

        var first = await session.NextEventAsync();
        Assert.NotNull(first);
        Assert.Equal(StreamEventKind.Stdout, first!.Kind);
        Assert.Equal("hi\n"u8.ToArray(), first.Data);

        await session.StopAsync(); // Cancel(reason "stop") → CancelAck skipped
        await session.StopAsync(); // idempotent

        var exit = await session.NextEventAsync(); // terminal Exit delivered after stop
        Assert.NotNull(exit);
        Assert.Equal(StreamEventKind.Exit, exit!.Kind);
        Assert.Equal(-1, exit.Code);

        Assert.Equal("stop", server.LastCancelReason);

        Assert.Null(await session.NextEventAsync());
        await Assert.ThrowsAsync<RemoteException>(() => session.SendInputAsync("late"));
    }

    [Fact]
    public async Task Stream_StderrAndExit()
    {
        using var server = new FakeZbrtServer();
        using var client = new ZbrtTcpClient(server.Address, Timeout);
        await client.ConnectAsync();
        var session = await client.StreamAsync(new[] { "echo", "hi" }, "/workspace");

        var stdout = await session.NextEventAsync();
        Assert.Equal(StreamEventKind.Stdout, stdout!.Kind);
        var stderr = await session.NextEventAsync();
        Assert.Equal(StreamEventKind.Stderr, stderr!.Kind);
        Assert.Equal("e\n"u8.ToArray(), stderr.Data);
        var exit = await session.NextEventAsync();
        Assert.Equal(StreamEventKind.Exit, exit!.Kind);
        Assert.Equal(0, exit.Code);
        Assert.Null(await session.NextEventAsync());
    }

    [Fact]
    public async Task RequestIds_AreFreshAndEchoed()
    {
        using var server = new FakeZbrtServer();
        using var client = new ZbrtTcpClient(server.Address, Timeout);
        await client.ConnectAsync();
        var first = await client.HealthAsync();
        var second = await client.HealthAsync();
        Assert.True(first);
        Assert.True(second);
        Assert.False(ZbrtTcpClient.NewRequestId().SequenceEqual(ZbrtTcpClient.NewRequestId()));
    }

    [Fact]
    public async Task ConnectionRefused_MapsToTransportError()
    {
        using var client = new ZbrtTcpClient("127.0.0.1:1", Timeout);
        var e = await Assert.ThrowsAsync<TransportException>(() => client.ConnectAsync());
        // WSL2's localhost NAT turns an unbound port into a timeout
        // ("guest connect timeout") rather than an immediate reset
        // ("connect failed") — both are valid transport failures.
        Assert.True(
            e.Message.Contains("connect failed") || e.Message.Contains("guest connect timeout"),
            $"unexpected transport message: {e.Message}");
    }
}

/// <summary>
/// ZBRT fail-closed contract for eval: ZBRT v1 has no eval opcode and the
/// reference guest maps Execute verbatim onto <c>exec</c>, so the old
/// "facade convention" (argv=["eval", code]) surfaced the guest's
/// <c>eval: not found</c> exit code as a successful result. The facade now
/// rejects eval over ZBRT locally, with zero frames on the wire.
/// </summary>
public class EvalZbrtVectorTests
{
    [Fact]
    public async Task Eval_OverZbrt_FailsClosedWithoutSendingFrames()
    {
        using var guest = new FakeZbrtServer();
        var (sandbox, controller) = await NewSandboxOverZbrtAsync(guest);
        using (controller)
        {
            await Assert.ThrowsAsync<ValidationException>(
                () => sandbox.Eval("1+1", cwd: "/workspace", timeoutS: 5));
            await Assert.ThrowsAsync<ValidationException>(() => sandbox.Eval("print(40+2)"));

            // Local validation still runs first and also stays off the wire.
            await Assert.ThrowsAsync<ValidationException>(() => sandbox.Eval("   "));
            await Assert.ThrowsAsync<ValidationException>(() => sandbox.Eval("1", timeoutS: 0));
            await Assert.ThrowsAsync<ValidationException>(() => sandbox.Eval("1", timeoutS: -1));
            await Assert.ThrowsAsync<ValidationException>(
                () => sandbox.Eval("1", timeoutS: double.PositiveInfinity));

            Assert.Equal(0, guest.AcceptCount); // fail closed — nothing reached the wire
        }
    }

    private static async Task<(Sandbox Sandbox, FakeHttpServer Controller)> NewSandboxOverZbrtAsync(
        FakeZbrtServer guest)
    {
        var body = """{"id":"sb-1","snapshot_tag":"base","guest_addr":"__ADDR__","has_branched":false,"branch_count":0}"""
            .Replace("__ADDR__", guest.Address);
        var controller = new FakeHttpServer(_ => new FakeHttpServer.Response(200, $"[{body}]"));
        var client = new RfbClient(controller.Url, token: null);
        var sandboxes = await client.CreateSandbox("base", transport: "zbrt");
        return (sandboxes[0], controller);
    }
}
