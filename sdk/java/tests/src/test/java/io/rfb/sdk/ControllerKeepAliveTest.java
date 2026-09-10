package io.rfb.sdk;

import io.rfb.sdk.internal.ControllerHttp;
import org.junit.jupiter.api.Test;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.time.Duration;
import java.util.List;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

import static org.junit.jupiter.api.Assertions.assertEquals;

/**
 * TCP regression: one {@link ControllerHttp} (one {@code java.net.http.HttpClient})
 * must reuse a single keep-alive connection for sequential controller requests —
 * N requests, exactly 1 accept. Mirrors the Rust reqwest connection pool.
 */
class ControllerKeepAliveTest {
    /** Raw HTTP/1.1 keep-alive server: one accept loop, per-socket request loop. */
    private static final class FakeKeepAliveServer implements AutoCloseable {
        final ServerSocket serverSocket;
        final ExecutorService pool = Executors.newCachedThreadPool();
        final AtomicInteger accepts = new AtomicInteger(0);
        volatile boolean closed = false;

        FakeKeepAliveServer() throws IOException {
            serverSocket = new ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"));
            Thread acceptor = new Thread(this::acceptLoop, "fake-keepalive-acceptor");
            acceptor.setDaemon(true);
            acceptor.start();
        }

        String baseUrl() {
            return "http://127.0.0.1:" + serverSocket.getLocalPort();
        }

        private void acceptLoop() {
            while (!closed) {
                try {
                    Socket socket = serverSocket.accept();
                    accepts.incrementAndGet();
                    pool.submit(() -> serve(socket));
                } catch (IOException e) {
                    if (!closed) {
                        throw new RuntimeException(e);
                    }
                    return;
                }
            }
        }

        /** Answer GETs on one connection until the peer closes. */
        private void serve(Socket socket) {
            try (socket) {
                InputStream in = socket.getInputStream();
                OutputStream out = socket.getOutputStream();
                byte[] body = "[{\"tag\":\"base\",\"status\":\"ready\",\"bootable\":true}]"
                        .getBytes(java.nio.charset.StandardCharsets.UTF_8);
                byte[] head = ("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                        + "Content-Length: " + body.length + "\r\n\r\n")
                        .getBytes(java.nio.charset.StandardCharsets.UTF_8);
                while (readRequestHead(in)) {
                    out.write(head);
                    out.write(body);
                    out.flush();
                }
            } catch (IOException ignored) {
                // peer went away — fine for a fake
            }
        }

        /** Consume one request head (request line + headers). False on clean EOF. */
        private static boolean readRequestHead(InputStream in) throws IOException {
            ByteArrayOutputStream buf = new ByteArrayOutputStream();
            int b;
            while ((b = in.read()) >= 0) {
                buf.write(b);
                int size = buf.size();
                if (size >= 4) {
                    byte[] raw = buf.toByteArray();
                    if (raw[size - 4] == '\r' && raw[size - 3] == '\n'
                            && raw[size - 2] == '\r' && raw[size - 1] == '\n') {
                        return true;
                    }
                }
            }
            return false;
        }

        @Override
        public void close() {
            closed = true;
            try {
                serverSocket.close();
            } catch (IOException ignored) {
                // best effort
            }
            pool.shutdownNow();
            try {
                pool.awaitTermination(2, TimeUnit.SECONDS);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
    }

    @Test
    void sequentialControllerRequestsReuseOneConnection() throws Exception {
        try (FakeKeepAliveServer server = new FakeKeepAliveServer()) {
            ControllerHttp http = new ControllerHttp(server.baseUrl(), null, Duration.ofSeconds(5));
            for (int i = 0; i < 5; i++) {
                List<Snapshot> snapshots = http.listSnapshots();
                assertEquals(1, snapshots.size());
                assertEquals("base", snapshots.get(0).tag);
            }
            // All 5 requests must have ridden one pooled keep-alive connection.
            assertEquals(1, server.accepts.get());
        }
    }
}
