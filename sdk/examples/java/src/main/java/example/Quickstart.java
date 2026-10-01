package example;

import io.rfb.sdk.DirEntry;
import io.rfb.sdk.ExecResult;
import io.rfb.sdk.RfbClient;
import io.rfb.sdk.Sandbox;
import io.rfb.sdk.SandboxInfo;

import java.util.Arrays;

/**
 * Quickstart: the same five sandbox calls on forkd (default, controller
 * snapshot) or zeroboot (direct ZBRT attach). Backends and run matrix:
 * see ../README.md.
 *
 * Run: mvn -q compile exec:java \
 *        -Dexec.mainClass=example.Quickstart -Dexec.args="--backend zeroboot"
 */
public final class Quickstart {
    public static void main(String[] args) throws Exception {
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
        boolean owned = !"zeroboot".equals(backend);
        Sandbox sandbox;
        if (owned) {
            sandbox = client.createSandbox(client.waitSnapshot(tag).getTag()).get(0);
        } else {
            // Direct attach: the bridge speaks ZBRT at the guest agent; the
            // bridge's runner owns the VM — nothing to create or delete.
            String tcp = System.getenv().getOrDefault("RFB_ZBRT_TCP",
                    "127.0.0.1:15000");
            SandboxInfo info = new SandboxInfo();
            info.setId("zeroboot-direct");
            info.setGuestAddr(tcp);
            sandbox = Sandbox.attach(client, info, RfbClient.TRANSPORT_ZBRT);
        }
        System.out.println("sandbox " + sandbox.id() + " via " + backend);

        try {
            System.out.println("ping: " + sandbox.ping());
            ExecResult result = sandbox.exec(
                    Arrays.asList("echo", "hello"), "/workspace", 60.0);
            System.out.println("exec: exit=" + result.getExitCode()
                    + " stdout=" + result.stdoutText().trim());
            System.out.println("written: " + sandbox.write("notes.txt",
                    "hello from rfb-sdk".getBytes()) + " bytes");
            System.out.println("read back "
                    + sandbox.read("notes.txt").getData().length + " bytes");
            for (DirEntry entry : sandbox.ls("/workspace")) {
                System.out.println("ls: " + entry.getName());
            }
        } finally {
            if (owned) {
                // Guarded: a mid-flow failure must not leak a live sandbox.
                sandbox.delete();
                System.out.println("sandbox deleted");
            }
        }
    }
}
