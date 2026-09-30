// Quickstart for the published Rfb.Sdk package (NuGet).
//
// Prerequisites: a running forkd controller (FORKD_URL, default
// http://127.0.0.1:8889) and a ready + bootable snapshot created with
// `rfb-cli forkd snapshot-create --tag rfb --tap forkd-tap0`. Run the
// example ON THE CONTROLLER'S NETWORK — guests live on the host-local
// TAP subnet (10.42.0.0/24), not routed to remote clients.
//
// Run: dotnet run -- rfb

using Rfb.Sdk;

var tag = args.Length > 0 ? args[0] : "rfb";

// FORKD_URL / FORKD_TOKEN / 10s timeout are the defaults.
var client = new RfbClient();

// Block until the snapshot reports status=ready and bootable=true.
var snapshot = await client.WaitSnapshot(tag);
Console.WriteLine($"snapshot {snapshot.Tag} is ready");

var sandboxes = await client.CreateSandbox(tag);
var sandbox = sandboxes[0];
Console.WriteLine($"sandbox {sandbox.Id} created");
try
{
    // Guarded: a mid-flow failure must not leak a live sandbox (it jams the
    // shared TAP for the next create).
    await Flow(sandbox);
}
finally
{
    await sandbox.Delete();
    Console.WriteLine("sandbox deleted");
}

async Task Flow(Sandbox sandbox)
{
    Console.WriteLine($"ping: {await sandbox.Ping()}");

    var result = await sandbox.Exec(["echo", "hello"], "/workspace");
    Console.WriteLine($"exec: exit={result.ExitCode} stdout={result.StdoutText.TrimEnd()}");

    var written = await sandbox.Write("notes.txt", "hello from rfb-sdk"u8.ToArray());
    Console.WriteLine($"written: {written} bytes");

    var file = await sandbox.Read("notes.txt");
    Console.WriteLine($"read back {file.Data.Length} bytes");

    foreach (var entry in await sandbox.Ls("/workspace"))
    {
        Console.WriteLine($"  {entry.Name}{(entry.IsDir ? "/" : "")}");
    }
}
