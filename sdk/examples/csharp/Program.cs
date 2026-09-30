// Quickstart for the published Rfb.Sdk package (NuGet) — both transports,
// one flow (the UNIFIED_API contract: identical shapes on either backend).
//
// Prerequisites:
//   forkd (default): a running controller (FORKD_URL, default
//     http://127.0.0.1:8889) with a ready snapshot created with
//     `rfb-cli forkd snapshot-create --tag rfb --tap forkd-tap0`;
//   zeroboot: a running ZBRT bridge (RFB_ZBRT_TCP, default 127.0.0.1:15000).
//   Run the example ON THE CONTROLLER'S NETWORK — guests live on the
//   host-local TAP subnet (10.42.0.0/24), not routed to remote clients.
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
if (backend == "zeroboot")
{
    // Direct attach: the bridge speaks ZBRT at the guest agent; no controller,
    // so nothing to create or delete — the bridge's runner owns the VM.
    var tcp = Environment.GetEnvironmentVariable("RFB_ZBRT_TCP") ?? "127.0.0.1:15000";
    var info = new SandboxInfo { Id = "zeroboot-direct", GuestAddr = tcp };
    var direct = Sandbox.Attach(client, info, RfbClient.TransportZbrt);
    Console.WriteLine($"direct ZBRT sandbox at {tcp}");
    await Flow(direct);
}
else
{
    // Block until the snapshot reports status=ready and bootable=true.
    var snapshot = await client.WaitSnapshot(tag);
    Console.WriteLine($"snapshot {snapshot.Tag} is ready");

    var sandboxes = await client.CreateSandbox(tag);
    var sandbox = sandboxes[0];
    Console.WriteLine($"sandbox {sandbox.Id} created");
    try
    {
        // Guarded: a mid-flow failure must not leak a live sandbox.
        await Flow(sandbox);
    }
    finally
    {
        await sandbox.Delete();
        Console.WriteLine("sandbox deleted");
    }
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
