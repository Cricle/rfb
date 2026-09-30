// Quickstart for the published Rfb.Sdk package (NuGet) — both transports,
// one flow (the UNIFIED_API contract: identical shapes on either backend).
// The flow body is shared data: this file interprets
// sdk/shared/conformance/example-flow.json, the same file every other
// language's quickstart reads.
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

using System.Text;
using System.Text.Json;
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
    var spec = JsonDocument.Parse(File.ReadAllText(FindSpec())).RootElement;
    foreach (var op in spec.GetProperty("ops").EnumerateArray())
    {
        switch (op.GetProperty("op").GetString())
        {
            case "ping":
                Console.WriteLine($"ping: {await sandbox.Ping()}");
                break;
            case "exec":
            {
                var argv = op.GetProperty("argv").EnumerateArray()
                    .Select(a => a.GetString()!).ToArray();
                var cwd = op.TryGetProperty("cwd", out var c) ? c.GetString()! : "/workspace";
                var result = await sandbox.Exec(argv, cwd);
                Console.WriteLine($"exec: exit={result.ExitCode} stdout={result.StdoutText.TrimEnd()}");
                break;
            }
            case "write":
            {
                var written = await sandbox.Write(
                    op.GetProperty("path").GetString()!,
                    Encoding.UTF8.GetBytes(op.GetProperty("text").GetString()!));
                Console.WriteLine($"written: {written} bytes");
                break;
            }
            case "read":
            {
                var file = await sandbox.Read(op.GetProperty("path").GetString()!);
                Console.WriteLine($"read back {file.Data.Length} bytes");
                break;
            }
            case "ls":
            {
                var names = (await sandbox.Ls(op.GetProperty("path").GetString()!))
                    .Select(e => e.Name).ToList();
                Console.WriteLine($"ls: [{string.Join(", ", names)}]");
                break;
            }
            default:
                throw new InvalidOperationException($"unknown op {op.GetProperty("op").GetString()}");
        }
    }
}

// Locate the shared spec: search upward from the working directory.
static string FindSpec()
{
    for (var dir = new DirectoryInfo(Environment.CurrentDirectory); dir != null; dir = dir.Parent)
    {
        foreach (var rel in new[] { "shared/conformance/example-flow.json",
                     "sdk/shared/conformance/example-flow.json" })
        {
            var p = Path.Combine(dir.FullName, rel);
            if (File.Exists(p)) return p;
        }
    }
    throw new FileNotFoundException("example-flow.json not found above "
        + Environment.CurrentDirectory);
}
