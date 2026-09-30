"""Host-side sandbox orchestration: run the BACKEND from Python.

The protocol adapters (see the package docstring) speak to a RUNNING stack.
This module brings the stack up and takes it down — without the Rust CLI:

- :class:`ZerobootHost` boots one Firecracker microVM directly (the UDS
  management API), bridges its vsock relay UDS onto TCP (Firecracker's
  ``CONNECT <guest-port>`` preamble protocol), and hands out a ZBRT client.
- :class:`ForkdHost` spawns the forkd controller, converges the shared TAP
  via ``ip(8)``, creates the snapshot through the official ``forkd`` binary,
  and hands out the :class:`RfbClient` facade.

Standard library only. The external process dependencies are the caller's
binaries (firecracker, forkd-controller, forkd) plus ``ip(8)`` for the TAP.

Both hosts follow the same lifecycle: :meth:`alive` (health), :meth:`up`
(idempotent convergence — a healthy stack is reused, stale leftovers from a
crashed previous run are reaped), :meth:`down`, and :meth:`client`.
"""

import http.client
import json
import os
import selectors
import socket
import subprocess
import threading
import time

from ._zbrt import _ZbrtGuestClient as _ZbrtClient
from .errors import RfbError
from .facade import RfbClient

__all__ = ["ForkdHost", "TcpVsockRelay", "ZerobootHost", "boot_firecracker"]

# Firecracker exposes guest virtio-vsock through a host Unix socket that is
# NOT a transparent byte stream: the client writes `CONNECT <guest-port>\n`
# and the relay answers `OK <host-port>\n` (or `ERR <code>\n`); after the
# handshake the connection carries guest AF_VSOCK stream bytes.
_RELAY_MAX_LINE_BYTES = 256

# Matches the Rust reference `BootArgs::new().random_trust_cpu().init(..)`;
# Firecracker appends the virtio_mmio.device line itself.
ZB_BOOT_ARGS = "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=1 init=/init"


# ---------------------------------------------------------------------------
# Firecracker management API over its Unix socket
# ---------------------------------------------------------------------------


class _UnixHTTP(http.client.HTTPConnection):
    """http.client over a UDS: Firecracker's management plane is one path."""

    def __init__(self, path: str):
        super().__init__("firecracker")
        self._uds = path

    def connect(self):
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(10)
        sock.connect(self._uds)
        self.sock = sock


def fc_api_put(sock_path: str, path: str, body: dict) -> None:
    """One Firecracker API PUT; anything outside 2xx raises with the body."""
    conn = _UnixHTTP(sock_path)
    try:
        conn.request(
            "PUT", path, body=json.dumps(body),
            headers={"Content-Type": "application/json"},
        )
        response = conn.getresponse()
        text = response.read().decode("utf-8", "replace")
        status = response.status
    finally:
        conn.close()
    if not 200 <= status < 300:
        raise RfbError(f"Firecracker API {path}: HTTP {status} {text}")


def _attach_pdeathsig() -> None:
    """Kernel-kills the spawned Firecracker when this process dies
    (PR_SET_PDEATHSIG, the same hygiene as the Rust `attach_pdeathsig`):
    a killed REPL must not orphan VMs."""
    try:
        import ctypes

        libc = ctypes.CDLL("libc.so.6", use_errno=True)
        libc.prctl(1, 9)  # PR_SET_PDEATHSIG, SIGKILL
    except Exception:
        pass  # hygiene, not correctness (same stance as the Rust side)


def boot_firecracker(fc_bin: str, kernel: str, rootfs: str, work_dir: str, *,
                     cid: int, uds_path: str, mem_size_mib: int = 512,
                     boot_args: str = ZB_BOOT_ARGS) -> subprocess.Popen:
    """Configure and start one VM through the Firecracker API.

    Every wait is bounded: the API socket (10s), each PUT (10s), and the
    vsock UDS appearing after InstanceStart (5s). On any failure the spawned
    process is killed before raising. Returns the live process handle (the
    caller owns its lifetime).
    """
    os.makedirs(work_dir, exist_ok=True)
    for stale in (os.path.join(work_dir, "firecracker.sock"), uds_path):
        try:
            os.remove(stale)
        except FileNotFoundError:
            pass
    sock_path = os.path.join(work_dir, "firecracker.sock")
    log = open(os.path.join(work_dir, "firecracker.log"), "ab")
    process = subprocess.Popen(
        [fc_bin, "--api-sock", sock_path],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=log, start_new_session=True)
    _attach_pdeathsig()
    try:
        deadline = time.time() + 10
        while not os.path.exists(sock_path):
            if time.time() > deadline or process.poll() is not None:
                raise RfbError("Firecracker API socket did not become ready")
            time.sleep(0.1)
        fc_api_put(sock_path, "/boot-source",
                   {"kernel_image_path": kernel, "boot_args": boot_args})
        fc_api_put(sock_path, "/drives/rootfs",
                   {"drive_id": "rootfs", "path_on_host": rootfs,
                    "is_root_device": True, "is_read_only": True})
        fc_api_put(sock_path, "/machine-config",
                   {"vcpu_count": 1, "mem_size_mib": mem_size_mib,
                    "smt": False})
        fc_api_put(sock_path, "/vsock",
                   {"guest_cid": cid, "uds_path": uds_path})
        fc_api_put(sock_path, "/actions", {"action_type": "InstanceStart"})
        deadline = time.time() + 5
        while not os.path.exists(uds_path):
            if time.time() > deadline:
                raise RfbError("vsock UDS did not appear after InstanceStart")
            time.sleep(0.1)
        return process
    except Exception:
        process.kill()
        process.wait()
        raise


# ---------------------------------------------------------------------------
# TCP → vsock relay UDS bridge
# ---------------------------------------------------------------------------


def _pump_bidirectional(a: socket.socket, b: socket.socket,
                        stop: threading.Event) -> None:
    """Copy between two sockets until either side ends or `stop` is set."""
    selector = selectors.DefaultSelector()
    a.setblocking(False)
    b.setblocking(False)
    selector.register(a, selectors.EVENT_READ)
    selector.register(b, selectors.EVENT_READ)
    peers = {a: b, b: a}
    try:
        while not stop.is_set():
            for key, _ in selector.select(0.5):
                source = key.fileobj
                try:
                    data = source.recv(65536)
                except (BlockingIOError, InterruptedError):
                    continue
                except OSError:
                    return
                if not data:
                    return
                try:
                    peers[source].sendall(data)
                except OSError:
                    return
    finally:
        selector.close()


class TcpVsockRelay:
    """TCP listener → Firecracker vsock relay UDS.

    Every accepted TCP connection dials the UDS, speaks the
    ``CONNECT <guest-port>`` preamble, validates the ``OK`` reply, and then
    pumps bytes both ways until either side disconnects.
    """

    def __init__(self, host: str, tcp_port: int, uds_path: str, guest_port: int):
        self.host = host
        self.tcp_port = tcp_port
        self.uds_path = uds_path
        self.guest_port = guest_port
        self._stop = threading.Event()
        self._listener = None

    def start(self) -> None:
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind((self.host, self.tcp_port))
        listener.listen(16)
        listener.settimeout(0.5)
        self._listener = listener
        threading.Thread(target=self._accept_loop, daemon=True).start()

    def stop(self) -> None:
        self._stop.set()
        if self._listener:
            try:
                self._listener.close()
            except OSError:
                pass

    def _accept_loop(self) -> None:
        while not self._stop.is_set():
            try:
                client, _ = self._listener.accept()
            except (socket.timeout, TimeoutError):
                continue
            except OSError:
                return
            threading.Thread(target=self._bridge, args=(client,),
                             daemon=True).start()

    def _bridge(self, client: socket.socket) -> None:
        with client:
            try:
                uds = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                uds.settimeout(10)
                uds.connect(self.uds_path)
                uds.sendall(f"CONNECT {self.guest_port}\n".encode())
                line = b""
                while not line.endswith(b"\n"):
                    byte = uds.recv(1)
                    if not byte:
                        return  # startup race: the guest had not bound yet
                    line += byte
                if not line.startswith(b"OK "):
                    return  # relay rejected (guest not listening, ...)
            except OSError:
                return
            with uds:
                _pump_bidirectional(client, uds, self._stop)


# ---------------------------------------------------------------------------
# Zeroboot host: one VM + TCP bridge, ZBRT clients direct
# ---------------------------------------------------------------------------


class ZerobootHost:
    """One Firecracker VM bridged onto TCP for direct ZBRT clients.

    No controller, no TAP. `run_dir` is the working directory (pid/socket
    files); `up` is idempotent — a healthy stack (ZBRT ping answers) is
    reused, and a stale VM from a crashed previous run is reaped first
    (pidfile + a run-dir-scoped pkill, never a global one).
    """

    def __init__(self, *, tcp: str, kernel: str, rootfs: str,
                 firecracker: str, run_dir: str,
                 guest_cid: int = 3, guest_port: int = 5000,
                 mem_size_mib: int = 512):
        host, _, port = tcp.rpartition(":")
        self.host = host or "127.0.0.1"
        self.tcp_port = int(port)
        self.tcp = f"{self.host}:{self.tcp_port}"
        self.kernel = kernel
        self.rootfs = rootfs
        self.firecracker = firecracker
        self.run_dir = run_dir
        self.guest_cid = guest_cid
        self.guest_port = guest_port
        self.mem_size_mib = mem_size_mib
        self._process = None
        self._relay = None

    def alive(self) -> bool:
        try:
            return _ZbrtClient(self.tcp, timeout_s=5.0).ping()
        except Exception:
            return False

    def client(self):
        """A ZBRT client bound to the bridged TCP address."""
        return _ZbrtClient(self.tcp, timeout_s=120.0)

    def _kill_stale(self) -> None:
        pidfile = os.path.join(self.run_dir, "firecracker.pid")
        if os.path.exists(pidfile):
            try:
                os.kill(int(open(pidfile).read().strip()), 9)
            except (ValueError, OSError):
                pass
            os.remove(pidfile)
        subprocess.run(["pkill", "-f", f"firecracker.*{self.run_dir}"],
                       capture_output=True)
        subprocess.run(["pkill", "-9", "-f", f"firecracker.*{self.run_dir}"],
                       capture_output=True)

    def up(self) -> None:
        if self.alive():
            return
        self._kill_stale()
        time.sleep(0.5)
        uds_path = os.path.join(self.run_dir, "vm", "vsock.sock")
        self._process = boot_firecracker(
            self.firecracker, self.kernel, self.rootfs,
            os.path.join(self.run_dir, "vm"),
            cid=self.guest_cid, uds_path=uds_path,
            mem_size_mib=self.mem_size_mib)
        with open(os.path.join(self.run_dir, "firecracker.pid"), "w") as pid:
            pid.write(str(self._process.pid))
        self._relay = TcpVsockRelay(self.host, self.tcp_port, uds_path,
                                    self.guest_port)
        self._relay.start()
        for _ in range(60):
            if self.alive():
                return
            time.sleep(1)
        self.down()
        raise RfbError(
            f"zeroboot backend did not become healthy on {self.tcp} "
            f"(firecracker log: {self.run_dir}/vm/firecracker.log)")

    def down(self) -> None:
        if self._relay:
            self._relay.stop()
        if self._process:
            self._process.kill()
            self._process.wait()
            self._process = None
        self._kill_stale()


# ---------------------------------------------------------------------------
# Forkd host: controller + TAP + snapshot, RfbClient facade
# ---------------------------------------------------------------------------


def default_snapshot_root() -> str:
    """Where `forkd snapshot` stores tags and the controller lists them from
    (`$XDG_DATA_HOME/forkd/snapshots`, else `$HOME/.local/share/...`) — the
    two must agree or the controller lists an empty set."""
    data = os.environ.get("XDG_DATA_HOME") or os.path.join(
        os.path.expanduser("~"), ".local", "share")
    return os.path.join(data, "forkd", "snapshots")


class ForkdHost:
    """The forkd stack: controller process + TAP + snapshot + facade.

    `up` converges idempotently: a reachable controller is never touched
    (re-running must not slaughter the live VMs it owns); a dead stack's
    leftovers are reaped (run-dir-scoped) before respawning. The snapshot is
    created through the official `forkd` binary and registered with the
    controller; a tag that already exists is reused as-is.
    """

    def __init__(self, *, url: str, tag: str, kernel: str, rootfs: str,
                 firecracker: str, controller_bin: str, forkd_bin: str,
                 run_dir: str, tap: str = "forkd-tap0",
                 snapshot_root: str = None):
        self.url = url
        self.bind = url.removeprefix("http://")
        self.tag = tag
        self.kernel = kernel
        self.rootfs = rootfs
        self.firecracker = firecracker
        self.controller_bin = controller_bin
        self.forkd_bin = forkd_bin
        self.run_dir = run_dir
        self.tap = tap
        self.snapshot_root = snapshot_root or default_snapshot_root()
        self._process = None

    def _env(self):
        # The forkd binaries resolve the kernel via FORKD_KERNEL and
        # firecracker via PATH (the e2e workflow's job-env convention).
        return dict(os.environ,
                    PATH=f"{os.path.dirname(self.firecracker)}:"
                         f"{os.environ.get('PATH', '')}",
                    FORKD_KERNEL=self.kernel)

    def alive(self) -> bool:
        import urllib.request

        try:
            with urllib.request.urlopen(f"{self.url}/v1/snapshots", timeout=3):
                return True
        except Exception:
            return False

    def client(self) -> RfbClient:
        return RfbClient(self.url, timeout_s=120.0)

    def _ensure_tap(self) -> None:
        """Idempotent TAP convergence: create if missing, add the 10.42.0.1/24
        address if absent, then bring it up (root-only system config; same
        semantics as the Rust `ensure_tap`)."""
        def ip(*args, check=False):
            return subprocess.run(["ip", *args], capture_output=True, text=True,
                                  check=check)

        if ip("link", "show", self.tap).returncode != 0:
            ip("tuntap", "add", "dev", self.tap, "mode", "tap", check=True)
        if "10.42.0.1" not in ip("addr", "show", "dev", self.tap).stdout:
            ip("addr", "add", "10.42.0.1/24", "dev", self.tap, check=True)
        ip("link", "set", self.tap, "up")

    def _kill_stale(self) -> None:
        for pattern in (f"firecracker.*{self.run_dir}",
                        "forkd-controller serve"):
            subprocess.run(["pkill", "-f", pattern], capture_output=True)
            subprocess.run(["pkill", "-9", "-f", pattern], capture_output=True)

    def _wait_controller(self, timeout_s: int) -> None:
        for _ in range(timeout_s):
            if self.alive():
                return
            time.sleep(1)
        raise RfbError(
            f"controller did not become healthy on {self.url} "
            f"(log: {self.run_dir}/state/controller.log)")

    def _spawn_controller(self) -> None:
        os.makedirs(self.snapshot_root, exist_ok=True)
        state = os.path.join(self.run_dir, "state")
        os.makedirs(state, exist_ok=True)
        log = open(os.path.join(state, "controller.log"), "ab")
        self._process = subprocess.Popen(
            [self.controller_bin, "serve",
             "--state", os.path.join(state, "state.json"),
             "--audit-log", os.path.join(state, "audit.log"),
             "--snapshot-root", self.snapshot_root,
             "--bind", self.bind],
            stdout=log, stderr=log, env=self._env(),
            start_new_session=True)

    def up(self) -> None:
        if not self.alive():
            self._kill_stale()
            time.sleep(0.5)
        self._ensure_tap()
        if not self.alive():
            self._spawn_controller()
        self._wait_controller(30)
        client = self.client()
        # Converge: the shared TAP carries exactly one live sandbox; one left
        # behind by a previous session would jam create with a 503.
        for stale in client.list_sandboxes():
            try:
                client.delete_sandbox(getattr(stale, "id", stale))
            except Exception:
                pass
        if client.snapshot(self.tag) is None:
            # The official forkd binary boots, pauses, stores and registers
            # the snapshot; the rootfs is copied per tag so the asset stays
            # pristine. --boot-wait-secs bounds the origin boot.
            private_rootfs = os.path.join(self.run_dir,
                                          f"{self.tag}-rootfs.ext4")
            if not os.path.exists(private_rootfs):
                import shutil

                shutil.copy(self.rootfs, private_rootfs)
            subprocess.run(
                [self.forkd_bin, "snapshot",
                 "--tag", self.tag,
                 "--kernel", self.kernel,
                 "--rootfs", private_rootfs,
                 "--tap", self.tap,
                 "--boot-wait-secs", "30",
                 "--daemon-url", self.url],
                env=self._env(), check=True)
        client.wait_snapshot(self.tag)

    def down(self) -> None:
        if self._process:
            self._process.terminate()
            try:
                self._process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self._process.kill()
            self._process = None
        self._kill_stale()
