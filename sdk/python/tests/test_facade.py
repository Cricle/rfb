"""Facade tests (UNIFIED_API.md section 9) running the same scenarios over
BOTH transports (ndjson default and zbrt) with identical result shapes."""

import json
import socket
import unittest

import rfb_sdk
from rfb_sdk import (
    DirEntry,
    ExecResult,
    FileRead,
    GrepMatch,
    GuestStream,
    HttpStatusError,
    RemoteError,
    RfbClient,
    Sandbox,
    SandboxInfo,
    Snapshot,
    StreamEventKind,
    TransportError,
    ValidationError,
    DecodeError,
)

from tests.fake_servers import FakeControllerServer, FakeNdjsonGuestServer, FakeZbrtServer


def _closed_port():
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


class FacadeMixin:
    """Shared scenarios; subclasses pin the transport and the fake guest."""

    transport = "ndjson"

    def _make_guest(self):
        raise NotImplementedError

    def setUp(self):
        self.guest = self._make_guest()
        self.guest.start()
        self.addCleanup(self.guest.stop)
        self.controller = FakeControllerServer(
            snapshots=[{"tag": "base", "status": "ready", "bootable": True}],
            snapshot_by_tag={
                "base": {"tag": "base", "status": "ready", "bootable": True},
                "legacy": {"tag": "legacy", "status": "ready", "bootable": True},
            },
            info_404_tags={"legacy"},
            guest_addr=f"127.0.0.1:{self.guest.port}",
        )
        self.controller.start()
        self.addCleanup(self.controller.stop)
        self.client = RfbClient(base_url=self.controller.url, token="tok", timeout_s=5.0)

    def _sandbox(self):
        sandboxes = self.client.create_sandbox("base", transport=self.transport)
        self.assertEqual(len(sandboxes), 1)
        sandbox = sandboxes[0]
        self.assertIsInstance(sandbox, Sandbox)
        self.assertEqual(sandbox.snapshot_tag, "base")
        return sandbox

    # -- scenarios (identical for both transports) -------------------------

    def test_ctor_env_defaults(self):
        client = RfbClient(base_url=self.controller.url)
        self.assertEqual(client.timeout_s, 10.0)

    def test_create_sandbox_returns_sandboxes(self):
        sandboxes = self.client.create_sandbox(
            "base", n=2, memory_limit_mib=512, per_child_netns=True, transport=self.transport
        )
        self.assertEqual(len(sandboxes), 2)
        sent = json.loads(self.controller.requests[-1]["body"])
        self.assertEqual(
            sent,
            {
                "snapshot_tag": "base",
                "n": 2,
                "per_child_netns": True,
                "memory_limit_mib": 512,
                "prewarm": False,
                "live_fork": False,
                "hugepages": False,
            },
        )
        # Bearer header present (token was passed to the client).
        self.assertEqual(
            self.controller.requests[-1]["headers"].get("Authorization"), "Bearer tok"
        )

    def test_exec(self):
        sandbox = self._sandbox()
        result = sandbox.exec(["echo", "hi"])
        self.assertIsInstance(result, ExecResult)
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(result.stdout, b"hello\n")
        self.assertEqual(result.stderr, b"")
        self.assertFalse(result.timed_out)
        self.assertEqual(result.stdout_text, "hello\n")
        self.assertEqual(result.stderr_text, "")

    def test_exec_with_stdin_and_timeout(self):
        sandbox = self._sandbox()
        result = sandbox.exec(["cat"], timeout_s=5.0, stdin=b"xyz")
        self.assertEqual(result.exit_code, 0)

    def test_eval_output_maps_to_stdout(self):
        sandbox = self._sandbox()
        result = sandbox.eval("print(42)")
        self.assertIsInstance(result, ExecResult)
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(result.stdout, b"eval out\n")
        self.assertEqual(result.stderr, b"")
        self.assertFalse(result.timed_out)

    def test_eval_validation(self):
        sandbox = self._sandbox()
        with self.assertRaises(ValidationError):
            sandbox.eval("   ")
        with self.assertRaises(ValidationError):
            sandbox.eval("ok", timeout_s=0)

    def test_ls_find_grep(self):
        sandbox = self._sandbox()
        entries = sandbox.ls()
        self.assertEqual(entries, [DirEntry(name="a.txt", is_dir=False, size=3)])
        found = sandbox.find(pattern="*.txt")
        self.assertEqual(found, ["a.txt"])
        matches = sandbox.grep(pattern="hi")
        self.assertEqual(matches, [GrepMatch(path="a.txt", line=1, column=1, text="hi")])

    def test_read_write(self):
        sandbox = self._sandbox()
        written = sandbox.write("out.txt", b"hi")
        self.assertEqual(written, 2)
        read = sandbox.read("out.txt")
        self.assertEqual(read, FileRead(data=b"hi", truncated=False, total_bytes=2))
        self.assertEqual(read.data, b"hi")
        self.assertFalse(read.truncated)
        self.assertEqual(read.total_bytes, 2)

    def test_read_validation(self):
        sandbox = self._sandbox()
        with self.assertRaises(ValidationError):
            sandbox.read("a\\b")
        with self.assertRaises(ValidationError):
            sandbox.read("ok.txt", max_bytes=0)
        with self.assertRaises(ValidationError):
            sandbox.read("ok.txt", max_bytes=51201)

    def test_write_validation(self):
        sandbox = self._sandbox()
        with self.assertRaises(ValidationError):
            sandbox.write("../escape", b"x")
        with self.assertRaises(ValidationError):
            sandbox.write("big.bin", b"x" * 51201)

    def test_ping(self):
        sandbox = self._sandbox()
        self.assertTrue(sandbox.ping())

    def test_delete(self):
        sandbox = self._sandbox()
        self.assertIsNone(sandbox.delete())
        deletes = [r for r in self.controller.requests if r["method"] == "DELETE"]
        self.assertEqual(len(deletes), 1)
        self.assertEqual(deletes[0]["path"], f"/v1/sandboxes/{sandbox.id}")

    def test_validation_fails_closed(self):
        sandbox = self._sandbox()
        connections_before = self.guest.connections
        received_before = len(getattr(self.guest, "received", []) or [])
        for call in (
            lambda: sandbox.ls("../escape"),
            lambda: sandbox.find(pattern=""),
            lambda: sandbox.grep(pattern="x" * 1025),
            lambda: sandbox.exec([]),
            lambda: sandbox.stream([]),
        ):
            with self.assertRaises(ValidationError):
                call()
        self.assertEqual(self.guest.connections, connections_before)
        self.assertEqual(len(getattr(self.guest, "received", []) or []), received_before)

    def test_remote_error(self):
        self.guest.fail_next = "boom"
        sandbox = self._sandbox()
        with self.assertRaises(RemoteError) as ctx:
            sandbox.exec(["x"])
        self.assertIn("boom", str(ctx.exception))
        self.guest.fail_next = None


class FacadeNdjsonTests(FacadeMixin, unittest.TestCase):
    transport = "ndjson"

    def _make_guest(self):
        return FakeNdjsonGuestServer(read_total=2)

    # -- ndjson-specific facade scenarios ----------------------------------

    def test_stream_session(self):
        sandbox = self._sandbox()
        stream = sandbox.stream(["cat"])
        self.assertIsInstance(stream, GuestStream)
        started = stream.next_event()
        self.assertEqual(started.kind, StreamEventKind.STARTED)
        stream.send_input("abc\n")
        stdout = stream.next_event()
        self.assertEqual(stdout.kind, StreamEventKind.STDOUT)
        self.assertEqual(stdout.data, b"abc\n")
        stream.stop()
        exit_event = stream.next_event()
        self.assertEqual(exit_event.kind, StreamEventKind.EXIT)
        self.assertEqual(exit_event.code, 130)
        self.assertIsNone(stream.next_event())
        with self.assertRaises(RemoteError):
            stream.send_input("late")
        stream.stop()  # idempotent

    def test_guest_error_line_is_remote_error(self):
        self.guest.error_response = "guest says no"
        sandbox = self._sandbox()
        with self.assertRaises(RemoteError):
            sandbox.ping()

    def test_ping_requires_pong_true(self):
        # Rust baseline (ndjson::ping_healthy): healthy only when the pong
        # flag is present and true — a `healthy` field alone never makes the
        # sandbox healthy, and pong=false reads as unhealthy.
        sandbox = self._sandbox()
        self.guest.ping_response = {"pong": True, "healthy": False}
        self.assertTrue(sandbox.ping())
        self.guest.ping_response = {"pong": False}
        self.assertFalse(sandbox.ping())
        self.guest.ping_response = {"healthy": True}
        self.assertFalse(sandbox.ping())

    def test_exec_null_exit_code_defaults_to_minus_one(self):
        # Rust baseline (ndjson::exec_result): a null / non-integer exit_code
        # on the terminal line falls back to -1 instead of raising.
        self.guest.exec_null_exit_code = True
        sandbox = self._sandbox()
        result = sandbox.exec(["echo", "hi"])
        self.assertEqual(result.exit_code, -1)
        self.assertEqual(result.stdout, b"hello\n")

    def test_oversize_line_is_decode_error(self):
        self.guest.oversize_response = True
        sandbox = self._sandbox()
        with self.assertRaises(DecodeError):
            sandbox.ping()


class FacadeZbrtTests(FacadeMixin, unittest.TestCase):
    transport = "zbrt"

    def _make_guest(self):
        return FakeZbrtServer()

    # -- zbrt-specific facade scenarios -------------------------------------

    def test_stream_pty_rejected_fail_closed(self):
        # Rust baseline (client/facade.rs GuestOps::stream, ZBRT arm): pty is
        # not supported over ZBRT -> Validation before any frame is sent.
        sandbox = self._sandbox()
        connections_before = self.guest.connections
        with self.assertRaises(ValidationError):
            sandbox.stream(["tail", "-f"], pty=True)
        self.assertEqual(self.guest.connections, connections_before)
        self.assertEqual(self.guest.received_executes, [])

    def test_stream_env_rejected_fail_closed(self):
        # Rust baseline: non-empty env is not supported over ZBRT -> Validation
        # before any frame is sent.
        sandbox = self._sandbox()
        connections_before = self.guest.connections
        with self.assertRaises(ValidationError):
            sandbox.stream(["tail", "-f"], env={"FOO": "bar"})
        self.assertEqual(self.guest.connections, connections_before)
        self.assertEqual(self.guest.received_executes, [])

    def test_stream_pty_false_and_empty_env_allowed(self):
        # Mirror the Rust boundary exactly: only pty==Some(true) and a
        # non-empty env object are rejected.
        sandbox = self._sandbox()
        sandbox.stream(["echo"], pty=False, env={})
        self.assertEqual(len(self.guest.received_executes), 1)

    def test_stream_output_then_exit(self):
        sandbox = self._sandbox()
        stream = sandbox.stream(["echo"])
        stdout = stream.next_event()
        self.assertEqual(stdout.kind, StreamEventKind.STDOUT)
        self.assertEqual(stdout.data, b"hello\n")
        exit_event = stream.next_event()
        self.assertEqual(exit_event.kind, StreamEventKind.EXIT)
        self.assertEqual(exit_event.code, 0)
        self.assertIsNone(stream.next_event())
        stream.stop()  # idempotent after terminal

    def test_stream_stop_cancels(self):
        self.guest.hold_execute_open = True
        sandbox = self._sandbox()
        stream = sandbox.stream(["tail", "-f"])
        stream.stop()  # Cancel -> CancelAck
        exit_event = stream.next_event()
        self.assertEqual(exit_event.kind, StreamEventKind.EXIT)
        self.assertEqual(exit_event.code, -1)
        stream.stop()  # idempotent
        self.assertEqual(len(self.guest.received_cancels), 1)

    def test_send_input_unsupported_over_zbrt(self):
        sandbox = self._sandbox()
        stream = sandbox.stream(["tail", "-f"])
        with self.assertRaises(RemoteError):
            stream.send_input("x")
        stream.stop()

    def test_zbrt_error_frame_is_remote_error(self):
        self.guest.fail_execute_message = "argv is empty"
        sandbox = self._sandbox()
        with self.assertRaises(RemoteError):
            sandbox.exec(["x"])


class FacadeSharedTests(unittest.TestCase):
    """Controller-side facade behavior (transport independent)."""

    def setUp(self):
        self.guest = FakeNdjsonGuestServer()
        self.guest.start()
        self.addCleanup(self.guest.stop)
        self.controller = FakeControllerServer(
            snapshots=[{"tag": "base", "status": "ready", "bootable": True}],
            snapshot_by_tag={"base": {"tag": "base", "status": "ready", "bootable": True}},
            guest_addr=f"127.0.0.1:{self.guest.port}",
        )
        self.controller.start()
        self.addCleanup(self.controller.stop)
        self.client = RfbClient(base_url=self.controller.url, timeout_s=5.0)

    def test_list_snapshots(self):
        snapshots = self.client.list_snapshots()
        self.assertEqual(len(snapshots), 1)
        self.assertIsInstance(snapshots[0], Snapshot)
        self.assertEqual(snapshots[0].tag, "base")

    def test_snapshot_info_and_fallback_and_none(self):
        snapshot = self.client.snapshot("base")
        self.assertEqual(snapshot.tag, "base")
        self.assertIsNone(self.client.snapshot("missing"))

    def test_wait_snapshot_ready(self):
        snapshot = self.client.wait_snapshot("base", timeout_s=2)
        self.assertEqual(snapshot.tag, "base")

    def test_wait_snapshot_failed_raises_remote_error(self):
        self.controller.snapshots = [{"tag": "base", "status": "FAILED", "bootable": False}]
        with self.assertRaises(RemoteError):
            self.client.wait_snapshot("base", timeout_s=2)

    def test_wait_snapshot_timeout_raises_transport_error(self):
        self.controller.snapshots = [{"tag": "base", "status": "building", "bootable": False}]
        with self.assertRaises(TransportError):
            self.client.wait_snapshot("base", timeout_s=1)

    def test_snapshot_dto_defaults(self):
        self.controller.behavior = lambda m, p, b: (
            (200, json.dumps({"tag": "minimal"}).encode()) if p == "/v1/snapshots/minimal" else None
        )
        self.controller.snapshot_by_tag["minimal"] = {"tag": "minimal"}
        snapshot = self.client.snapshot("minimal")
        self.assertEqual(snapshot.tag, "minimal")
        self.assertEqual(snapshot.dir, "")
        self.assertIsNone(snapshot.created_at_unix)
        self.assertEqual(snapshot.status, "")
        self.assertFalse(snapshot.bootable)
        self.assertIsNone(snapshot.provenance)

    def test_sandbox_info_defaults(self):
        self.controller.behavior = lambda m, p, b: (
            (200, json.dumps([{"id": "sb-9", "snapshot_tag": "base"}]).encode())
            if m == "GET" and p == "/v1/sandboxes"
            else None
        )
        sandboxes = self.client.list_sandboxes()
        info = sandboxes[0].info
        self.assertIsInstance(info, SandboxInfo)
        self.assertEqual(info.id, "sb-9")
        self.assertEqual(info.guest_addr, "")
        self.assertFalse(info.has_branched)
        self.assertEqual(info.branch_count, 0)
        self.assertIsNone(info.pid)

    def test_connect_by_id(self):
        sandbox = self.client.create_sandbox("base")[0]
        attached = self.client.connect(sandbox.id)
        self.assertIsInstance(attached, Sandbox)
        self.assertEqual(attached.id, sandbox.id)
        self.assertEqual(attached.guest_addr, sandbox.guest_addr)

    def test_connect_by_sandbox_object(self):
        sandbox = self.client.create_sandbox("base")[0]
        attached = self.client.connect(sandbox)
        self.assertEqual(attached.id, sandbox.id)

    def test_connect_preserves_sandbox_transport(self):
        # Rust attach semantics: a Sandbox is attached as-is; only an
        # explicitly passed transport overrides it.
        sandbox = self.client.create_sandbox("base", transport="zbrt")[0]
        attached = self.client.connect(sandbox)
        self.assertEqual(attached.id, sandbox.id)
        self.assertEqual(attached.transport, "zbrt")
        overridden = self.client.connect(sandbox, transport="ndjson")
        self.assertEqual(overridden.transport, "ndjson")

    def test_connect_unknown_id_raises_remote_error(self):
        with self.assertRaises(RemoteError):
            self.client.connect("nope")

    def test_connect_rejects_bad_id(self):
        with self.assertRaises(ValidationError):
            self.client.connect("bad id!")

    def test_ping_sandbox_returns_raw_value(self):
        sandbox = self.client.create_sandbox("base")[0]
        self.assertEqual(self.client.ping_sandbox(sandbox.id), {"pong": True})

    def test_delete_sandbox(self):
        sandbox = self.client.create_sandbox("base")[0]
        self.assertIsNone(self.client.delete_sandbox(sandbox.id))
        self.controller.delete_404_ids.add("already-gone-404")
        self.assertIsNone(self.client.delete_sandbox("already-gone-404"))

    def test_http_status_error_mapping(self):
        self.controller.behavior = lambda m, p, b: (
            (500, json.dumps({"error": "boom"}).encode())
            if m == "POST" and p == "/v1/sandboxes"
            else None
        )
        with self.assertRaises(HttpStatusError) as ctx:
            self.client.create_sandbox("base")
        self.assertEqual(ctx.exception.status, 500)
        self.assertEqual(ctx.exception.message, "boom")

    def test_decode_error_mapping(self):
        self.controller.behavior = lambda m, p, b: (200, b"not json")
        with self.assertRaises(DecodeError):
            self.client.list_snapshots()

    def test_transport_error_on_dead_guest(self):
        sandbox = self.client.create_sandbox("base")[0]
        dead = Sandbox(
            SandboxInfo(
                id=sandbox.id,
                snapshot_tag="base",
                guest_addr=f"127.0.0.1:{_closed_port()}",
            ),
            self.client,
        )
        with self.assertRaises(TransportError):
            dead.ping()

    def test_invalid_transport_rejected(self):
        with self.assertRaises(ValidationError):
            self.client.create_sandbox("base", transport="smoke")
        with self.assertRaises(ValidationError):
            self.client.connect("sb-1", transport="smoke")

    def test_error_hierarchy(self):
        for exc_class in (
            TransportError,
            HttpStatusError,
            DecodeError,
            RemoteError,
            ValidationError,
        ):
            self.assertTrue(issubclass(exc_class, rfb_sdk.RfbError))


if __name__ == "__main__":
    unittest.main()
