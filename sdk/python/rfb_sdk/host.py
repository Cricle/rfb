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
from .models import SandboxInfo
from .facade import RfbClient, Sandbox

__all__ = ["ForkdHost", "TcpVsockRelay", "ZerobootHost", "boot_firecracker"]

# Firecracker exposes guest virtio-vsock through a host Unix socket that is
# NOT a transparent byte stream: the client writes `CONNECT <guest-port>\n`
# and the relay answers `OK <host-port>\n` (or `ERR <code>\n`); after the
# handshake the connection carries guest AF_VSOCK stream bytes.
_RELAY_MAX_LINE_BYTES = 256

# Matches the Rust reference `BootArgs::new().random_trust_cpu().init(..)`;
# Firecracker appends the virtio_mmio.device line itself.
ZB_BOOT_ARGS = "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=1 init=/init"


def restore_asset(assets_dir: str, name: str, target: str) -> str:
    """`assets_dir/<name>[.gz]` -> `target` (unwrapped or copied); returns the
    target. An existing target is left alone — a running binary cannot be
    overwritten (ETXTBSY); bump assets by taking the stack down first."""
    import gzip
    import shutil

    target = str(target)
    if os.path.exists(target):
        return target
    os.makedirs(os.path.dirname(target), exist_ok=True)
    packed = os.path.join(assets_dir, name + ".gz")
    plain = os.path.join(assets_dir, name)
    if os.path.exists(packed):
        with gzip.open(packed, "rb") as reader, open(target, "wb") as writer:
            shutil.copyfileobj(reader, writer)
    elif os.path.exists(plain):
        shutil.copy(plain, target)
    else:
        raise RfbError(f"missing asset: {name} (looked in {assets_dir})")
    os.chmod(target, 0o755)
    return target


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


def _pkill_all(pattern: str) -> None:
    """SIGTERM first, wait for the process table to drain, SIGKILL only the
    survivors. SIGKILL is last resort BY DESIGN: a SIGKILLed VM whose parent
    is gone becomes an unreapable zombie, and the controller's shared-TAP
    gate counts process-table users — a zombie jammed every later create
    with a 503 "in use by another live sandbox" (2026-09-30, hours of
    alternating green/red rounds)."""
    subprocess.run(["pkill", "-f", pattern], capture_output=True)
    for _ in range(20):
        probe = subprocess.run(["pgrep", "-f", pattern], capture_output=True)
        if probe.returncode != 0:
            return
        time.sleep(0.5)
    subprocess.run(["pkill", "-9", "-f", pattern], capture_output=True)


_subreaper_started = threading.Lock()
_subreaper_live = False


def _become_child_subreaper() -> None:
    """PR_SET_CHILD_SUBREAPER: orphaned descendants (the `forkd snapshot`
    daemon's parent VM survives the binary that spawned it) reparent to US
    instead of a non-reaping init. Without this, a SIGKILLed parent VM sits
    in the process table as an unreapable zombie — and the controller's
    shared-TAP gate counts every firecracker entry, zombie or live. One
    process-wide reaper thread serves every ForkdHost instance."""
    global _subreaper_live
    with _subreaper_started:
        if _subreaper_live:
            return
        try:
            import ctypes

            libc = ctypes.CDLL("libc.so.6", use_errno=True)
            if libc.prctl(36, 1) != 0:  # PR_SET_CHILD_SUBREAPER, 1
                return  # unsupported platform: skip reaping entirely
            threading.Thread(target=_reap_loop, daemon=True).start()
            _subreaper_live = True
        except Exception:
            pass  # hygiene, not correctness


def _reap_loop() -> None:
    """Drain reparented zombies so the process table stays clean.

    waitpid(-1, WNOHANG) returns (0, 0) when children exist but none have
    exited — that is the steady state for the whole stack lifetime, so it
    MUST back off (an unguarded loop spins at ~2M iters/s on one core).
    """
    while True:
        try:
            pid, _ = os.waitpid(-1, os.WNOHANG)
            if pid == 0:
                time.sleep(0.2)
        except ChildProcessError:
            time.sleep(1)
        except OSError:
            time.sleep(0.2)


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


def _pump_bidirectional(a: socket.socket, b: socket.socket) -> None:
    """Blocking two-thread copy: a select loop costs a poll syscall per
    direction switch; blocking recv/sendall release the GIL and ride the
    kernel directly — lower per-op latency under concurrency. EOF propagates
    via shutdown(SHUT_WR) so the peer pump ends too."""

    def _copy(src: socket.socket, dst: socket.socket) -> None:
        try:
            while True:
                data = src.recv(65536)
                if not data:
                    return
                dst.sendall(data)
        except OSError:
            return
        finally:
            try:
                dst.shutdown(socket.SHUT_WR)
            except OSError:
                pass

    t = threading.Thread(target=_copy, args=(b, a), daemon=True)
    t.start()
    try:
        _copy(a, b)
    finally:
        t.join(timeout=5)


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
        self._live = set()  # 活连接登记：stop 时强拆，泵的阻塞 recv 才能醒

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
        for sock in list(self._live):
            try:
                sock.shutdown(socket.SHUT_RDWR)
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
        client.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self._live.add(client)
        try:
            # The with-block covers the WHOLE handshake: every failure path
            # (relay not ready, rejected preamble, timeout) closes the UDS —
            # the boot window alone tries ~60 connects, one leaked fd per
            # attempt adds up fast.
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as uds:
                self._live.add(uds)
                try:
                    uds.settimeout(10)
                    uds.connect(self.uds_path)
                    uds.sendall(f"CONNECT {self.guest_port}\n".encode())
                    line = b""
                    while not line.endswith(b"\n"):
                        if len(line) >= _RELAY_MAX_LINE_BYTES:
                            return  # a relay must answer within one line
                        byte = uds.recv(1)
                        if not byte:
                            return  # startup race: the guest had not bound yet
                        line += byte
                    if not line.startswith(b"OK "):
                        return  # relay rejected (guest not listening, ...)
                except OSError:
                    return
                _pump_bidirectional(client, uds)
            self._live.discard(uds)
        finally:
            self._live.discard(client)
            try:
                client.close()
            except OSError:
                pass


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

    def __init__(self, *, tcp: str, run_dir: str, assets_dir: str = None,
                 kernel: str = None, rootfs: str = None, firecracker: str = None,
                 guest_cid: int = 3, guest_port: int = 5000,
                 mem_size_mib: int = 512):
        host, _, port = tcp.rpartition(":")
        self.host = host or "127.0.0.1"
        self.tcp_port = int(port)
        self.tcp = f"{self.host}:{self.tcp_port}"
        self.run_dir = run_dir
        # Asset names are the convention; an explicit path overrides the
        # assets-dir resolution per slot. Neither = a hard error (a None
        # assets_dir would otherwise die with a raw TypeError).
        self.firecracker = firecracker or restore_asset(
            assets_dir, "firecracker", f"{run_dir}/firecracker")
        self.kernel = kernel or restore_asset(
            assets_dir, "vmlinux", f"{run_dir}/vmlinux")
        self.rootfs = rootfs or restore_asset(
            assets_dir, "zeroboot-zbrt.ext4", f"{run_dir}/rootfs.ext4")
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
        """A raw ZBRT client bound to the bridged TCP address."""
        return _ZbrtClient(self.tcp, timeout_s=120.0)

    def sandbox(self) -> Sandbox:
        """The unified facade `Sandbox` over the bridged ZBRT transport.

        Identical method shapes to the forkd facade (`exec` -> `ExecResult`,
        `ls` -> `DirEntry`, ...) — the UNIFIED_API contract holds across both
        transports; callers cannot tell this sandbox apart from a
        controller-created one.
        """
        info = SandboxInfo(id="zeroboot-direct", snapshot_tag="zeroboot-zbrt",
                           guest_addr=self.tcp)
        # The zbrt facade path only reads the client's timeout; the
        # controller handle is inert (no requests are ever made).
        return Sandbox(info, RfbClient(timeout_s=120.0), transport="zbrt")

    def _kill_stale(self) -> None:
        pidfile = os.path.join(self.run_dir, "firecracker.pid")
        if os.path.exists(pidfile):
            try:
                pid = int(open(pidfile).read().strip())
                with open(f"/proc/{pid}/cmdline", "rb") as cmdline:
                    ident = cmdline.read().replace(b"\0", b" ").decode(
                        "utf-8", "replace")
                # Only signal what IS our firecracker: after a reboot the pid
                # may belong to an unrelated process.
                if "firecracker" in ident:
                    os.kill(pid, 15)
            except (ValueError, OSError):
                pass
            os.remove(pidfile)
        _pkill_all(f"firecracker.*{self.run_dir}")

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
        try:
            with open(os.path.join(self.run_dir, "firecracker.pid"), "w") as pid:
                pid.write(str(self._process.pid))
            self._relay = TcpVsockRelay(self.host, self.tcp_port, uds_path,
                                        self.guest_port)
            self._relay.start()
        except BaseException:
            self.down()  # a failed bind must not leak a running VM
            raise
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

    def __init__(self, *, url: str, tag: str, run_dir: str,
                 assets_dir: str = None, kernel: str = None, rootfs: str = None,
                 firecracker: str = None, controller_bin: str = None,
                 forkd_bin: str = None, tap: str = "forkd-tap0",
                 snapshot_root: str = None):
        self.url = url
        self.bind = url.removeprefix("http://")
        self.tag = tag
        self.run_dir = run_dir
        self.firecracker = firecracker or restore_asset(
            assets_dir, "firecracker", f"{run_dir}/firecracker")
        self.kernel = kernel or restore_asset(
            assets_dir, "vmlinux", f"{run_dir}/vmlinux")
        self.rootfs = rootfs or restore_asset(
            assets_dir, "forkd-agent.ext4", f"{run_dir}/rootfs.ext4")
        self.controller_bin = controller_bin or restore_asset(
            assets_dir, "forkd-controller", f"{run_dir}/forkd-controller")
        self.forkd_bin = forkd_bin or restore_asset(
            assets_dir, "forkd", f"{run_dir}/forkd")
        self.tap = tap
        self.snapshot_root = snapshot_root or default_snapshot_root()
        self._process = None
        _become_child_subreaper()

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
        # The controller gets SIGTERM first: it reaps its own VMs. Killing
        # the VMs with SIGKILL first would orphan-zombie them (see
        # _pkill_all) and jam the shared-TAP gate.
        _pkill_all("forkd-controller serve")
        _pkill_all(f"firecracker.*{self.run_dir}")
        # The `forkd snapshot` helper daemon (forkd-daemon-<tag>-*) keeps its
        # origin VM alive on the shared TAP for live-fork; a daemon left over
        # from an interrupted run makes every later create 503 ("shared host
        # tap is in use by another live sandbox"). Its VM is not ours to
        # manage — take the daemon down with the stack.
        _pkill_all("forkd-daemon-")

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
            import glob as _glob

            for stale_dir in _glob.glob("/tmp/forkd-daemon-*"):
                import shutil

                shutil.rmtree(stale_dir, ignore_errors=True)
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
        # Existence check goes through the LIST endpoint (the Rust reference's
        # snapshot_ready semantics): the per-tag detail route is optional and
        # older controllers 405 it.
        if not any(getattr(item, "tag", None) == self.tag
                   for item in client.list_snapshots()):
            # The official forkd binary boots, pauses, stores and registers
            # the snapshot. The rootfs is copied per tag INTO THE SNAPSHOT
            # DATA DIR (the Rust reference's copy_rootfs_private: the copy is
            # part of the snapshot's persistent state — a run-dir copy dies
            # with every run-dir wipe and leaves the snapshot pointing at a
            # deleted file). Copy to a temp name + os.replace so a crash
            # mid-copy can never leave a truncated rootfs behind.
            # --boot-wait-secs bounds the origin boot.
            private_rootfs = os.path.join(self.snapshot_root, self.tag,
                                          "rootfs.ext4")
            if not os.path.exists(private_rootfs):
                import shutil

                os.makedirs(os.path.dirname(private_rootfs), exist_ok=True)
                staging = private_rootfs + ".partial"
                shutil.copy(self.rootfs, staging)
                os.replace(staging, private_rootfs)
            proc = subprocess.run(
                [self.forkd_bin, "snapshot",
                 "--tag", self.tag,
                 "--kernel", self.kernel,
                 "--rootfs", private_rootfs,
                 "--tap", self.tap,
                 "--boot-wait-secs", "30",
                 "--daemon-url", self.url],
                env=self._env(), capture_output=True, text=True)
            # The forkd binary narrates its own boot/snapshot/registration;
            # surface it so a "failed" status is never a mystery.
            if proc.stdout:
                print(proc.stdout.rstrip())
            if proc.stderr:
                print(proc.stderr.rstrip())
            if proc.returncode != 0:
                raise RfbError(f"forkd snapshot failed rc={proc.returncode}")
            # The snapshot's parent VM lingers (the live-fork template) and
            # the controller's shared-TAP gate counts it — retire it and wait
            # for the process table to drain before handing the stack over.
            _pkill_all("firecracker.*forkd-parent-")
            for _ in range(15):
                probe = subprocess.run(["pgrep", "-f", "firecracker.*forkd-parent-"],
                                       capture_output=True)
                if probe.returncode != 0:
                    break
                time.sleep(1)
        client.wait_snapshot(self.tag)

    def down(self) -> None:
        if self._process:
            self._process.terminate()
            try:
                self._process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self._process.kill()
            self._process = None
        # The controller (SIGTERMed above) reaps its own VMs; the sweep only
        # force-kills what survived, and never SIGKILLs a VM while its
        # parent might still reap it (see _pkill_all). The snapshot daemon
        # goes down WITH the stack — its parent VM holds the shared TAP and
        # jams every later create otherwise.
        _pkill_all("forkd-controller serve")
        _pkill_all(f"firecracker.*{self.run_dir}")
        _pkill_all("forkd-daemon-")
        import glob as _glob

        for stale_dir in _glob.glob("/tmp/forkd-daemon-*"):
            import shutil

            shutil.rmtree(stale_dir, ignore_errors=True)
