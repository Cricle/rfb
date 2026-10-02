"""Host-side orchestration unit tests — everything offline (no VMs)."""

import gzip
import os
import socket
import threading
import unittest

from rfb_sdk.errors import RfbError
from rfb_sdk.host import (
    TcpVsockRelay,
    ZerobootHost,
    _pump_to_tcp,
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


# ---------------------------------------------------------------------------
# expose/unexpose（guest→host 反向 vsock 的宿主侧）—— 离线回归
# ---------------------------------------------------------------------------

class ExposeTest(unittest.TestCase):
    """expose 的监听/转发/强拆语义（不需要真 VM——直接驱动内部件）。"""

    def _expose_via_host(self, host, guest_port, target):
        """绕过 alive() 门禁直接走 expose 的内部装配（真机路径的 E2E 在
        手工验证矩阵里）。"""
        import socket as s
        import threading
        uds_path = os.path.join(host.run_dir, "vm", "vsock.sock")
        reverse_path = f"{uds_path}_{guest_port}"
        try:
            os.unlink(reverse_path)
        except FileNotFoundError:
            pass
        server = s.socket(s.AF_UNIX, s.SOCK_STREAM)
        server.bind(reverse_path)
        server.listen(16)
        stop_event = threading.Event()
        live = set()

        def register(upstream, conn):
            live.add((upstream, conn))

        def acceptor():
            server.settimeout(0.5)
            while not stop_event.is_set():
                try:
                    conn, _ = server.accept()
                except (s.timeout, TimeoutError):
                    continue
                except OSError:
                    break
                threading.Thread(target=_pump_to_tcp,
                                 args=(conn, target, stop_event, register),
                                 daemon=True).start()
            server.close()

        threading.Thread(target=acceptor, daemon=True).start()

        def stop():
            stop_event.set()
            for upstream, conn in list(live):
                for sk in (upstream, conn):
                    try:
                        sk.shutdown(s.SHUT_RDWR)
                    except OSError:
                        pass
            live.clear()
            try:
                os.unlink(reverse_path)
            except OSError:
                pass

        return stop

    def test_target_unreachable_closes_guest_side(self):
        """target 连不上：guest 侧收到通用错误文案（无拓扑回显）+ 连接关闭。"""
        import socket as s
        h = self._host()
        uds_path = os.path.join(h.run_dir, "vm", "vsock.sock")
        reverse = uds_path + "_7777"
        stop = self._expose_via_host(h, 7777, "127.0.0.1:1")  # 端口 1 = 无人听
        client = s.socket(s.AF_UNIX, s.SOCK_STREAM)
        # WSL 的 loopback 对无人端口可能丢包（RST 不回）——pump 的 connect
        # 会等满 5s 超时；recv 窗口必须 > 5s 才与该路径赛赢。
        client.settimeout(10)
        client.connect(reverse)
        data = client.recv(256)
        self.assertEqual(data, b"host target unreachable\n")
        self.assertEqual(client.recv(256), b"")  # 连接关闭
        client.close()
        stop()

    def test_bad_target_port_never_leaks_fd(self):
        """target 端口非法（"host:"→ValueError / 99999→OverflowError）：
        与网络错误同路收尾，guest 侧拿到通用文案且连接关闭。"""
        import socket as s
        h = self._host()
        uds_path = os.path.join(h.run_dir, "vm", "vsock.sock")
        for target in ("127.0.0.1:", "127.0.0.1:99999"):
            reverse = uds_path + "_7778"
            try:
                os.unlink(reverse)
            except FileNotFoundError:
                pass
            stop = self._expose_via_host(h, 7778, target)
            client = s.socket(s.AF_UNIX, s.SOCK_STREAM)
            client.settimeout(5)
            client.connect(reverse)
            data = client.recv(256)
            self.assertEqual(data, b"host target unreachable\n")
            self.assertEqual(client.recv(256), b"")
            client.close()
            stop()

    def _host(self):
        import tempfile
        run_dir = tempfile.mkdtemp(prefix="rfb-expose-test-")
        os.makedirs(os.path.join(run_dir, "vm"), exist_ok=True)
        host = ZerobootHost.__new__(ZerobootHost)
        host.run_dir = run_dir
        host._exposures = {}
        return host
