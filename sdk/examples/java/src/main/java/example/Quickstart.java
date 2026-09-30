package example;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import io.rfb.sdk.DirEntry;
import io.rfb.sdk.ExecResult;
import io.rfb.sdk.RfbClient;
import io.rfb.sdk.Sandbox;
import io.rfb.sdk.SandboxInfo;
import io.rfb.sdk.Snapshot;

import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.List;

/**
 * Quickstart for the published io.github.cricle:rfb-sdk artifact (Maven
 * Central) — both transports, one flow (the UNIFIED_API contract: identical
 * shapes on either backend). The flow body is shared data: this file
 * interprets sdk/shared/conformance/example-flow.json, the same file every
 * other language's quickstart reads.
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

    /** The SAME calls on either backend — the scenario is shared data. */
    private static void flow(Sandbox sandbox) throws Exception {
        JsonNode ops = new ObjectMapper()
                .readTree(Files.readAllBytes(findSpec())).get("ops");
        for (JsonNode op : ops) {
            String kind = op.get("op").asText();
            if (kind.equals("ping")) {
                System.out.println("ping: " + sandbox.ping());
            } else if (kind.equals("exec")) {
                List<String> argv = new ArrayList<>();
                op.get("argv").forEach(a -> argv.add(a.asText()));
                String cwd = op.hasNonNull("cwd") ? op.get("cwd").asText()
                        : "/workspace";
                ExecResult result = sandbox.exec(argv, cwd, 60.0);
                System.out.println("exec: exit=" + result.getExitCode()
                        + " stdout=" + result.stdoutText().trim());
            } else if (kind.equals("write")) {
                System.out.println("written: " + sandbox.write(
                        op.get("path").asText(),
                        op.get("text").asText().getBytes()) + " bytes");
            } else if (kind.equals("read")) {
                System.out.println("read back "
                        + sandbox.read(op.get("path").asText()).getData().length
                        + " bytes");
            } else if (kind.equals("ls")) {
                List<String> names = new ArrayList<>();
                for (DirEntry entry : sandbox.ls(op.get("path").asText())) {
                    names.add(entry.getName());
                }
                System.out.println("ls: " + names);
            } else {
                throw new IllegalStateException("unknown op " + kind);
            }
        }
    }

    /** Locate the shared spec: search upward from the working directory. */
    private static Path findSpec() throws Exception {
        for (Path dir = Paths.get("").toAbsolutePath();
                dir != null; dir = dir.getParent()) {
            for (String rel : new String[] {"shared/conformance/example-flow.json",
                    "sdk/shared/conformance/example-flow.json"}) {
                Path candidate = dir.resolve(rel);
                if (Files.exists(candidate)) {
                    return candidate;
                }
            }
        }
        throw new IllegalStateException("example-flow.json not found above "
                + Paths.get("").toAbsolutePath());
    }
}
