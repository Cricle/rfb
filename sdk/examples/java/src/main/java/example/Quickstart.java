package example;

import io.rfb.sdk.RfbClient;
import io.rfb.sdk.RfbError;
import io.rfb.sdk.Sandbox;
import io.rfb.sdk.Snapshot;

/**
 * Quickstart for the published io.github.cricle:rfb-sdk artifact (Maven Central).
 *
 * Prerequisites: a running forkd controller (FORKD_URL, default
 * http://127.0.0.1:8889) and a ready + bootable snapshot created with
 * {@code rfb-cli forkd snapshot-create --tag rfb --tap forkd-tap0}.
 *
 * Run: mvn -q compile exec:java -Dexec.mainClass=example.Quickstart -Dexec.args="rfb"
 */
public final class Quickstart {
    public static void main(String[] args) {
        String tag = args.length > 0 ? args[0] : "rfb";

        // FORKD_URL / FORKD_TOKEN / 10s timeout are the defaults.
        RfbClient client = new RfbClient();

        // Block until the snapshot reports status=ready and bootable=true.
        Snapshot snapshot = client.waitSnapshot(tag);
        System.out.println("snapshot " + snapshot.tag + " is ready");

        Sandbox sandbox = client.createSandbox(tag).get(0);
        System.out.println("sandbox " + sandbox.id() + " created");

        System.out.println("ping: " + sandbox.ping());

        ExecResultPrinter.print(sandbox.exec(
                java.util.Arrays.asList("echo", "hello"), "/workspace", 60.0));

        int written = sandbox.write("notes.txt", "hello from rfb-sdk".getBytes());
        System.out.println("written: " + written + " bytes");

        System.out.println("read back " + sandbox.read("notes.txt").data.length + " bytes");

        for (io.rfb.sdk.DirEntry entry : sandbox.ls("/workspace")) {
            System.out.println("  " + entry.name + (entry.isDir ? "/" : ""));
        }

        sandbox.delete();
        System.out.println("sandbox deleted");
    }

    /** Tiny printer so the example stays dependency-free. */
    private static final class ExecResultPrinter {
        private ExecResultPrinter() {
        }

        static void print(io.rfb.sdk.ExecResult result) {
            System.out.println("exec: exit=" + result.exitCode
                    + " stdout=" + result.stdoutText().trim());
        }
    }
}
