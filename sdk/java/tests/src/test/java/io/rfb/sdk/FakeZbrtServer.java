package io.rfb.sdk;

import io.rfb.sdk.internal.ZbrtCodec;
import io.rfb.sdk.internal.ZbrtFrame;

import java.io.BufferedInputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;

/**
 * In-process fake ZBRT v1 guest: accepts TCP connections and hands each one a
 * {@link ConnHandler} that reads/writes raw {@link ZbrtFrame}s.
 */
final class FakeZbrtServer implements AutoCloseable {
    interface ConnHandler {
        void handle(FrameIO io) throws IOException;
    }

    /** Frame reader/writer over one accepted socket. */
    static final class FrameIO {
        final Socket socket;
        final java.io.InputStream in;
        final OutputStream out;

        FrameIO(Socket socket) throws IOException {
            this.socket = socket;
            this.in = new BufferedInputStream(socket.getInputStream());
            this.out = socket.getOutputStream();
        }

        ZbrtFrame read() throws IOException {
            try {
                return ZbrtFrame.decode(in);
            } catch (io.rfb.sdk.DecodeError e) {
                // The client closed mid-read (EOF) — expected fake teardown.
                // Surface as IOException so handlers/pool treat it as "client
                // went away" instead of an assertion failure.
                throw new IOException("client closed while reading a frame", e);
            }
        }

        void write(ZbrtFrame frame) throws IOException {
            out.write(frame.encode());
            out.flush();
        }

        void writeFrames(ZbrtFrame... frames) throws IOException {
            for (ZbrtFrame frame : frames) {
                write(frame);
            }
        }

        ZbrtFrame reply(ZbrtFrame request, int kind, byte[] payload) {
            return new ZbrtFrame(kind, 0, request.requestId(), payload);
        }
    }

    private final ServerSocket serverSocket;
    private final ExecutorService pool = Executors.newCachedThreadPool();
    private final List<ConnHandler> handlers = new ArrayList<>();
    private final List<Socket> sockets = new ArrayList<>();
    private final List<Throwable> handlerFailures = java.util.Collections.synchronizedList(new ArrayList<>());
    private volatile boolean closed = false;

    FakeZbrtServer(ConnHandler... perConnectionHandlers) throws IOException {
        this.serverSocket = new ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"));
        for (ConnHandler handler : perConnectionHandlers) {
            handlers.add(handler);
        }
        Thread acceptor = new Thread(this::acceptLoop, "fake-zbrt-acceptor");
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
                        handler.handle(new FrameIO(socket));
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

    static ZbrtFrame outputFrame(byte[] requestId, int stream, byte[] data) {
        return new ZbrtFrame(ZbrtFrame.KIND_OUTPUT, 0, requestId,
                ZbrtCodec.encodeOutput(stream, data));
    }

    static ZbrtFrame exitFrame(byte[] requestId, int code) {
        return new ZbrtFrame(ZbrtFrame.KIND_EXIT, 0, requestId, ZbrtCodec.encodeExit(code, null));
    }

    static ZbrtFrame errorFrame(byte[] requestId, long code, String message) {
        return new ZbrtFrame(ZbrtFrame.KIND_ERROR, 0, requestId, ZbrtCodec.encodeError(code, message));
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
            throw new AssertionError("fake zbrt handler failed", handlerFailures.get(0));
        }
    }
}
