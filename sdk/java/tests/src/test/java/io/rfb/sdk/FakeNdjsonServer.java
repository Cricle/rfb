package io.rfb.sdk;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.PrintWriter;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;

/**
 * In-process fake forkd guest: accepts TCP connections and hands each one to a
 * scripted {@link ConnHandler}. No external services, no sleeps.
 */
final class FakeNdjsonServer implements AutoCloseable {
    interface ConnHandler {
        void handle(BufferedReader in, PrintWriter out) throws IOException;
    }

    private final ServerSocket serverSocket;
    private final ExecutorService pool = Executors.newCachedThreadPool();
    private final List<ConnHandler> handlers = new ArrayList<>();
    private final List<Socket> sockets = new ArrayList<>();
    private final List<Throwable> handlerFailures = java.util.Collections.synchronizedList(new ArrayList<>());
    private volatile boolean closed = false;
    private final java.util.concurrent.atomic.AtomicInteger requestIndex =
            new java.util.concurrent.atomic.AtomicInteger();

    FakeNdjsonServer(ConnHandler... perConnectionHandlers) throws IOException {
        this.serverSocket = new ServerSocket(0, 50, java.net.InetAddress.getByName("127.0.0.1"));
        for (ConnHandler handler : perConnectionHandlers) {
            handlers.add(handler);
        }
        Thread acceptor = new Thread(this::acceptLoop, "fake-guest-acceptor");
        acceptor.setDaemon(true);
        acceptor.start();
    }

    String address() {
        return "127.0.0.1:" + serverSocket.getLocalPort();
    }

    /** Accepted connection count (fail-closed tests assert 0: no traffic). */
    int connectionCount() {
        synchronized (sockets) {
            return sockets.size();
        }
    }

    private void acceptLoop() {
        while (!closed) {
            try {
                Socket socket = serverSocket.accept();
                socket.setTcpNoDelay(true);
                synchronized (sockets) {
                    sockets.add(socket);
                }
                pool.submit(() -> {
                    try (socket) {
                        BufferedReader in = new BufferedReader(
                                new InputStreamReader(socket.getInputStream(), StandardCharsets.UTF_8));
                        PrintWriter out = new PrintWriter(
                                new java.io.BufferedWriter(new java.io.OutputStreamWriter(
                                        socket.getOutputStream(), StandardCharsets.UTF_8)));
                        // 连接复用模型（对齐真 agent 的 serve 循环）：每个
                        // 请求依序取下一个脚本化 handler；耗尽后停在最后一个
                        // （SDK 的连接池会让多个请求共享一条连接）。
                        while (true) {
                            int index = requestIndex.getAndIncrement();
                            ConnHandler handler = handlers.size() > 1
                                    ? handlers.get(Math.min(index, handlers.size() - 1))
                                    : handlers.get(0);
                            try {
                                handler.handle(in, out);
                            } catch (NullPointerException exhausted) {
                                // handler 读到 readLine()==null = 客户端已关：
                                // 连接结束（流的单请求 handler 没料到循环）。
                                break;
                            }
                        }
                    } catch (IOException ignored) {
                        // client went away — fine for a fake
                    } catch (Throwable failure) {
                        // Handler assertions run on pool threads: record them
                        // so close() can surface the failure instead of the
                        // test stalling and passing.
                        handlerFailures.add(failure);
                    }
                });
            } catch (IOException e) {
                if (closed) {
                    return;
                }
                throw new RuntimeException(e);
            }
        }
    }

    @Override
    public void close() {
        closed = true;
        try {
            serverSocket.close();
        } catch (IOException ignored) {
            // best effort
        }
        synchronized (sockets) {
            for (Socket socket : sockets) {
                try {
                    socket.close();
                } catch (IOException ignored) {
                    // best effort
                }
            }
        }
        pool.shutdownNow();
        try {
            pool.awaitTermination(2, TimeUnit.SECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        // Surface the first handler-side failure (assertions would otherwise be
        // swallowed by the pool and the test could pass with a broken guest).
        if (!handlerFailures.isEmpty()) {
            throw new AssertionError("fake guest handler failed", handlerFailures.get(0));
        }
    }
}
