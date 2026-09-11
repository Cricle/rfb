using System.Text;
using System.Text.Json;
using Rfb.Sdk.Internal;
using Xunit;

namespace Rfb.Sdk.Tests;

/// <summary>forkd guest NDJSON client against a fake TCP server (PROTOCOL.md §2).</summary>
public class GuestNdjsonTests
{
    private static readonly TimeSpan Timeout = TimeSpan.FromSeconds(5);

    private static ForkdGuestNdjson Client(FakeNdjsonGuest guest) =>
        new(guest.Address, Timeout);

    [Fact]
    public async Task Request_ReadsLinesUntilTerminal()
    {
        using var guest = new FakeNdjsonGuest();
        var responses = await Client(guest).RequestAsyncForTests(new Dictionary<string, object?> {
            ["action"] = "exec", ["cwd"] = "/", ["args"] = new[] { "echo", "hi" }, ["timeout"] = 60ul,
        });
        // fake sends {"progress":1} (non-terminal) then the terminal exec line
        Assert.Equal(2, responses.Count);
        Assert.True(responses[0].TryGetProperty("progress", out _));
        Assert.True(responses[1].TryGetProperty("exit_code", out _));
    }

    [Fact]
    public async Task Request_TwoLinesInOneSegment_BothRead()
    {
        // Root-cause regression: Linux loopback coalesces server writes into
        // one TCP segment; a stateless reader dropped everything after the
        // first line and then timed out waiting for a response that was
        // already consumed.
        static Task Handler(FakeNdjsonSession session, JsonElement value) =>
            session.WriteLinesAsync(
                new Dictionary<string, object?> { ["progress"] = 1 },
                new Dictionary<string, object?> { ["exit_code"] = 0, ["out"] = "hi" });

        using var guest = new FakeNdjsonGuest { Handler = Handler };
        var responses = await Client(guest).RequestAsyncForTests(new Dictionary<string, object?> {
            ["action"] = "exec", ["cwd"] = "/", ["args"] = new[] { "echo", "hi" }, ["timeout"] = 60ul,
        });
        Assert.Equal(2, responses.Count);
        Assert.True(responses[0].TryGetProperty("progress", out _));
        Assert.Equal(0, responses[1].GetProperty("exit_code").GetInt32());
    }

    [Fact]
    public async Task Exec_TerminalResponseMapped()
    {
        using var guest = new FakeNdjsonGuest();
        var v = await Client(guest).ExecAsync("/", new[] { "echo", "hi" }, 60);
        Assert.Equal(0, v.GetProperty("exit_code").GetInt32());
        Assert.Equal("hi\n", v.GetProperty("out").GetString());
    }

    [Fact]
    public async Task ErrorLine_RaisesRemoteError()
    {
        using var guest = new FakeNdjsonGuest();
        var e = await Assert.ThrowsAsync<RemoteException>(
            () => Client(guest).ExecAsync("/", new[] { "fail" }, 60));
        Assert.Contains("boom", e.Message);
    }

    [Fact]
    public async Task ErrorOnNonTerminalLine_RaisesImmediately()
    {
        using var guest = new FakeNdjsonGuest();
        var e = await Assert.ThrowsAsync<RemoteException>(
            () => Client(guest).ExecAsync("/", new[] { "slowfail" }, 60));
        Assert.Contains("late", e.Message);
    }

    [Fact]
    public async Task OversizeLine_RaisesDecodeError()
    {
        static Task Handler(FakeNdjsonSession session, JsonElement value) =>
            session.WriteAsync(new Dictionary<string, object?> { ["junk"] = new string('x', 1100 * 1024) });

        using var guest = new FakeNdjsonGuest { Handler = Handler };
        // An oversized line is a decode failure (PROTOCOL.md; Python/Java match).
        var e = await Assert.ThrowsAsync<DecodeException>(() => Client(guest).PingAsync());
        Assert.Contains("exceeded 1048576 bytes", e.Message);
    }

    [Fact]
    public async Task Ping_TerminalPong()
    {
        using var guest = new FakeNdjsonGuest();
        var v = await Client(guest).PingAsync();
        Assert.True(v.GetProperty("pong").GetBoolean());
    }

    [Fact]
    public async Task Eval_IncludesOptionalTimeout()
    {
        using var guest = new FakeNdjsonGuest();
        var v = await Client(guest).EvalAsync("1+1", "/workspace", 2.4);
        Assert.Equal(0x32, v.GetProperty("output")[0].GetInt32());
        Assert.Equal(0, v.GetProperty("status").GetInt32());
    }

    [Fact]
    public async Task Stream_Session_EventsInputStop()
    {
        using var guest = new FakeNdjsonGuest();
        var stream = await Client(guest).StreamAsync(new[] { "cat" }, null, null, null);
        var started = await stream.NextEventAsync();
        Assert.NotNull(started);
        Assert.True(started.Value.TryGetProperty("started", out _));

        await stream.SendInputAsync("hello");
        var echo = await stream.NextEventAsync();
        Assert.Equal("echo:hello", echo.Value.GetProperty("stdout").GetString());

        await stream.StopAsync(); // idempotency checked by second call
        await stream.StopAsync();
        var exit = await stream.NextEventAsync();
        Assert.Equal(9, exit.Value.GetProperty("exit_code").GetInt32());

        await Assert.ThrowsAsync<RemoteException>(() => stream.SendInputAsync("late"));
    }

    [Fact]
    public async Task ClosedBeforeResponse_RaisesRemoteError()
    {
        using var guest = new FakeNdjsonGuest
        {
            Handler = (session, _) =>
            {
                session.Close();
                return Task.CompletedTask;
            },
        };
        var e = await Assert.ThrowsAsync<RemoteException>(() => Client(guest).PingAsync());
        Assert.Equal("guest closed before response", e.Message);
    }
}
