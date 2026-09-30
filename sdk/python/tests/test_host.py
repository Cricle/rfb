"""Host-side orchestration unit tests — everything offline (no VMs)."""

import gzip
import socket
import threading
import unittest

from rfb_sdk.errors import RfbError
from rfb_sdk.host import (
    TcpVsockRelay,
    _pump_bidirectional,
    default_snapshot_root,
    fc_api_put,
)


class FakeFirecracker:
    """A minimal stand-in for the vsock relay UDS: answers `CONNECT <p>\\n`
    with `OK <host-port>\\n`, then echoes bytes back (echo server)."""

    def __init__(self, path):
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(path)
        self.server.listen(4)
        self.server.settimeout(5)
        self.connects = []
        threading.Thread(target=self._serve, daemon=True).start()

    def _serve(self):
        while True:
            try:
                conn, _ = self.server.accept()
            except OSError:
                return
            threading.Thread(target=self._handle, args=(conn,),
                             daemon=True).start()

    def _handle(self, conn):
        line = b""
        while not line.endswith(b"\n"):
            byte = conn.recv(1)
            if not byte:
                conn.close()
                return
            line += byte
        self.connects.append(line.decode().strip())
        port = line.decode().split()[1]
        conn.sendall(f"OK {port}\n".encode())
        while True:
            data = conn.recv(65536)
            if not data:
                break
            conn.sendall(data)  # echo: the "guest" replies with the same bytes
        conn.close()


class RelayBridgeTest(unittest.TestCase):
    """The TCP→UDS bridge speaks the CONNECT/OK preamble, then pumps bytes."""

    def test_relay_bridges_tcp_to_the_uds_preamble_and_back(self):
        import os
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            uds = os.path.join(tmp, "vsock.sock")
            fake = FakeFirecracker(uds)
            relay = TcpVsockRelay("127.0.0.1", 0, uds, 5000)
            relay.guest_port = 5000
            relay.start()
            # The relay bound an ephemeral port only if we used port 0 — the
            # class binds exactly the given port, so use the listener's.
            port = relay._listener.getsockname()[1]
            with socket.create_connection(("127.0.0.1", port), timeout=5) as s:
                s.sendall(b"ping-from-tcp")
                self.assertEqual(s.recv(65536), b"ping-from-tcp")
            self.assertEqual(fake.connects, ["CONNECT 5000"])
            relay.stop()

    def test_preamble_rejection_closes_the_bridge_without_pumping(self):
        import os
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            uds = os.path.join(tmp, "vsock.sock")
            server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            server.bind(uds)
            server.listen(1)
            server.settimeout(5)

            def reject():
                conn, _ = server.accept()
                conn.recv(256)
                conn.sendall(b"ERR 1\n")
                conn.close()

            threading.Thread(target=reject, daemon=True).start()
            relay = TcpVsockRelay("127.0.0.1", 0, uds, 5000)
            relay.start()
            port = relay._listener.getsockname()[1]
            with socket.create_connection(("127.0.0.1", port), timeout=5) as s:
                s.settimeout(2)
                # A rejected preamble pumps nothing and the bridge closes:
                # the TCP client sees EOF, not a hang.
                self.assertEqual(s.recv(65536), b"")
            relay.stop()


class FirecrackerApiTest(unittest.TestCase):
    """fc_api_put surfaces non-2xx with the response text."""

    def test_error_status_raises_with_the_body(self):
        import os
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "fc.sock")
            server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            server.bind(path)
            server.listen(1)
            server.settimeout(5)

            def reply_400():
                conn, _ = server.accept()
                request = conn.recv(4096)
                body = b'{"fault_message":"bad drive"}'
                conn.sendall(
                    b"HTTP/1.1 400 Bad Request\r\n"
                    b"Content-Type: application/json\r\n"
                    + f"Content-Length: {len(body)}\r\n\r\n".encode()
                    + body)
                conn.close()

            threading.Thread(target=reply_400, daemon=True).start()
            with self.assertRaises(RfbError) as caught:
                fc_api_put(path, "/drives/rootfs", {"drive_id": "rootfs"})
            self.assertIn("HTTP 400", str(caught.exception))
            self.assertIn("bad drive", str(caught.exception))


class ForkdHostPathsTest(unittest.TestCase):
    """Snapshot root: XDG first, HOME fallback — controller and forkd binary
    must agree or the controller lists an empty set."""

    def test_xdg_data_home_wins(self):
        import os

        previous = os.environ.get("XDG_DATA_HOME")
        os.environ["XDG_DATA_HOME"] = "/tmp/xdg-test"
        try:
            self.assertEqual(default_snapshot_root(),
                             "/tmp/xdg-test/forkd/snapshots")
        finally:
            if previous is None:
                del os.environ["XDG_DATA_HOME"]
            else:
                os.environ["XDG_DATA_HOME"] = previous

    def test_home_fallback(self):
        import os

        previous = os.environ.pop("XDG_DATA_HOME", None)
        try:
            root = default_snapshot_root()
            self.assertTrue(root.endswith("forkd/snapshots"))
            self.assertNotIn("xdg-test", root)
        finally:
            if previous is not None:
                os.environ["XDG_DATA_HOME"] = previous


if __name__ == "__main__":
    unittest.main()
