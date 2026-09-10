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
    private volatile boolean closed = false;

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

    private void acceptLoop() {
        while (!closed) {
            try {
                Socket socket = serverSocket.accept();
                socket.setTcpNoDelay(true);
                synchronized (sockets) {
                    sockets.add(socket);
                }
                ConnHandler handler = handlers.size() > 1 ? handlers.remove(0) : handlers.get(0);
                pool.submit(() -> {
                    try (socket) {
                        BufferedReader in = new BufferedReader(
                                new InputStreamReader(socket.getInputStream(), StandardCharsets.UTF_8));
                        PrintWriter out = new PrintWriter(
                                new java.io.BufferedWriter(new java.io.OutputStreamWriter(
                                        socket.getOutputStream(), StandardCharsets.UTF_8)));
                        handler.handle(in, out);
                    } catch (IOException ignored) {
                        // client went away — fine for a fake
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
    }
}
