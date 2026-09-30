package example;

import io.rfb.sdk.DirEntry;
import io.rfb.sdk.ExecResult;
import io.rfb.sdk.RfbClient;
import io.rfb.sdk.Sandbox;
import io.rfb.sdk.SandboxInfo;
import io.rfb.sdk.Snapshot;

/**
 * Quickstart for the published io.github.cricle:rfb-sdk artifact (Maven
 * Central) — both transports, one flow (the UNIFIED_API contract: identical
 * shapes on either backend).
 *
 * Prerequisites:
 *   forkd (default): a running controller (FORKD_URL, default
 *     http://127.0.0.1:8889) with a ready snapshot created with
 *     {@code rfb-cli forkd snapshot-create --tag rfb --tap forkd-tap0};
 *   zeroboot: a running ZBRT bridge (RFB_ZBRT_TCP, default 127.0.0.1:15000).
 *
 * Run: mvn -q compile exec:java \
 *        -Dexec.mainClass=example.Quickstart -Dexec.args="--backend zeroboot"
 */
public final class Quickstart {
    public static void main(String[] args) {
        String backend = "forkd";
        String tag = "rfb";
        for (int i = 0; i < args.length; i++) {
            if ("--backend".equals(args[i]) && i + 1 < args.length) {
                backend = args[++i];
            } else {
                tag = args[i];
            }
        }

        // FORKD_URL / FORKD_TOKEN / 10s timeout are the defaults.
        RfbClient client = new RfbClient();
        if ("zeroboot".equals(backend)) {
            // Direct attach: the bridge speaks ZBRT at the guest agent; no
            // controller, so nothing to create or delete — the bridge's
            // runner owns the VM lifecycle.
            String tcp = System.getenv().getOrDefault("RFB_ZBRT_TCP",
                    "127.0.0.1:15000");
            SandboxInfo info = new SandboxInfo();
            info.setId("zeroboot-direct");
            info.setGuestAddr(tcp);
            Sandbox sandbox = Sandbox.attach(client, info,
                    RfbClient.TRANSPORT_ZBRT);
            System.out.println("direct ZBRT sandbox at " + tcp);
            flow(sandbox);
            return;
        }

        // Block until the snapshot reports status=ready and bootable=true.
        Snapshot snapshot = client.waitSnapshot(tag);
        System.out.println("snapshot " + snapshot.getTag() + " is ready");

        Sandbox sandbox = client.createSandbox(tag).get(0);
        System.out.println("sandbox " + sandbox.id() + " created");
        try {
            // Guarded: a mid-flow failure must not leak a live sandbox.
            flow(sandbox);
        } finally {
            sandbox.delete();
            System.out.println("sandbox deleted");
        }
    }

    /** The SAME calls on either backend — shapes never change. */
    private static void flow(Sandbox sandbox) {
        System.out.println("ping: " + sandbox.ping());

        ExecResult result = sandbox.exec(
                java.util.Arrays.asList("echo", "hello"), "/workspace", 60.0);
        System.out.println("exec: exit=" + result.getExitCode()
                + " stdout=" + result.stdoutText().trim());

        int written = sandbox.write("notes.txt", "hello from rfb-sdk".getBytes());
        System.out.println("written: " + written + " bytes");

        System.out.println("read back " + sandbox.read("notes.txt").getData().length
                + " bytes");

        for (DirEntry entry : sandbox.ls("/workspace")) {
            System.out.println("  " + entry.getName() + (entry.isDir() ? "/" : ""));
        }
    }
}
