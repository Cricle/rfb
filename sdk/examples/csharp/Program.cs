// Quickstart: the same five sandbox calls on forkd (default, controller
// snapshot) or zeroboot (direct ZBRT attach). Backends and run matrix:
// see ../README.md.
//
// Run: dotnet run -- [--backend zeroboot] [rfb]

using Rfb.Sdk;

var backend = "forkd";
var tag = "rfb";
for (var i = 0; i < args.Length; i++)
{
    if (args[i] == "--backend" && i + 1 < args.Length)
    {
        backend = args[++i];
    }
    else
    {
        tag = args[i];
    }
}

// FORKD_URL / FORKD_TOKEN / 10s timeout are the defaults.
var client = new RfbClient();
var owned = backend != "zeroboot";
Sandbox sandbox;
if (owned)
{
    sandbox = (await client.CreateSandbox((await client.WaitSnapshot(tag)).Tag))[0];
}
else
{
    // Direct attach: the bridge speaks ZBRT at the guest agent; the bridge's
    // runner owns the VM — nothing to create or delete.
    var tcp = Environment.GetEnvironmentVariable("RFB_ZBRT_TCP") ?? "127.0.0.1:15000";
    sandbox = Sandbox.Attach(client,
        new SandboxInfo { Id = "zeroboot-direct", GuestAddr = tcp },
        RfbClient.TransportZbrt);
}
Console.WriteLine($"sandbox {sandbox.Id} via {backend}");

try
{
    Console.WriteLine($"ping: {await sandbox.Ping()}");
    var result = await sandbox.Exec(["echo", "hello"], "/workspace");
    Console.WriteLine($"exec: exit={result.ExitCode} stdout={result.StdoutText.TrimEnd()}");
    Console.WriteLine($"written: {await sandbox.Write("notes.txt", "hello from rfb-sdk"u8.ToArray())} bytes");
    Console.WriteLine($"read back {(await sandbox.Read("notes.txt")).Data.Length} bytes");
    Console.WriteLine("ls: " + string.Join(", ", (await sandbox.Ls("/workspace")).Select(e => e.Name)));
}
finally
{
    if (owned)
    {
        // Guarded: a mid-flow failure must not leak a live sandbox.
        await sandbox.Delete();
        Console.WriteLine("sandbox deleted");
    }
}
