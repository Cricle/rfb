"""ZBRT guest client tests against a fake frame server (rfb_sdk._zbrt)."""

import socket
import struct
import unittest
from unittest import mock

from rfb_sdk import _zbrt as z
from rfb_sdk.errors import DecodeError, RemoteError, TransportError

from tests.fake_servers import FakeZbrtServer, read_frame, write_frame


def _closed_port():
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


class ZbrtClientTests(unittest.TestCase):
    def setUp(self):
        self.server = FakeZbrtServer()
        self.server.start()
        self.addCleanup(self.server.stop)
        self.client = z._ZbrtGuestClient(self.server.address, 5.0)

    def test_ping_health_ack(self):
        self.assertTrue(self.client.ping())

    def test_reply_with_mismatched_request_id_is_decode_error(self):
        # PROTOCOL.md §3.4: replies must echo the request id.
        self.server.mismatch_reply_id = True
        with self.assertRaises(DecodeError):
            self.client.ping()

    def test_stream_frame_with_mismatched_request_id_is_decode_error(self):
        self.server.mismatch_reply_id = True
        stream = self.client.open_stream(["cat"], "/")
        # First poll is the synthesized started event; the corrupted reply id
        # then hits the Output frame read.
        started = stream.next_event()
        self.assertEqual(started.kind.value, "started")
        with self.assertRaises(DecodeError):
            stream.next_event()

    def test_exec_collects_output_then_exit(self):
        code, stdout, stderr = self.client.exec(["echo", "hi"], "/", 10, b"")
        self.assertEqual(code, 0)
        self.assertEqual(stdout, b"hello\n")
        self.assertEqual(stderr, b"")
        argv, cwd, stdin, timeout_ms = self.server.received_executes[0]
        self.assertEqual(argv, ["echo", "hi"])
        self.assertEqual(cwd, "/")
        self.assertEqual(stdin, b"")
        self.assertEqual(timeout_ms, 10000)

    def test_exec_stderr_collected(self):
        self.server.exec_stderr = b"warn\n"
        _code, _stdout, stderr = self.client.exec(["echo", "hi"], "/", 10, b"")
        self.assertEqual(stderr, b"warn\n")

    def test_exec_error_frame_raises_remote_error(self):
        self.server.fail_execute_message = "argv is empty"
        with self.assertRaises(RemoteError) as ctx:
            self.client.exec([], "/", 10, b"")
        self.assertIn("argv is empty", str(ctx.exception))

    def test_exec_timeout_ms_ceil_to_whole_seconds(self):
        # Rust baseline: timeout_ms = ceil(secs) * 1000 with a minimum of 1s.
        self.client.exec(["echo"], "/", 1.4, b"")
        _argv, _cwd, _stdin, timeout_ms = self.server.received_executes[0]
        self.assertEqual(timeout_ms, 2000)
        self.client.exec(["echo"], "/", 0.2, b"")
        _argv, _cwd, _stdin, timeout_ms = self.server.received_executes[1]
        self.assertEqual(timeout_ms, 1000)

    def test_fs_roundtrip_all_ops(self):
        entries = self.client.fs_op(z.FS_OP_LS, ".", {"max_results": 1000})
        self.assertEqual(entries["entries"][0]["name"], "a.txt")
        self.assertEqual(self.server.received_fs_ops[-1], (1, ".", {"max_results": 1000}))

        matches = self.client.fs_op(z.FS_OP_FIND, ".", {"pattern": "*.txt", "max_results": 1000})
        self.assertEqual(matches["matches"], ["a.txt"])

        greps = self.client.fs_op(
            z.FS_OP_GREP, ".", {"pattern": "hi", "max_results": 1000, "max_bytes": 51200}
        )
        self.assertEqual(greps["matches"][0]["path"], "a.txt")

        read = self.client.fs_op(z.FS_OP_READ, "a.txt", {"offset": None, "max_bytes": None})
        self.assertEqual(bytes(read["data"]), b"hi")
        self.assertEqual(read["total_bytes"], 2)

        written = self.client.fs_op(
            z.FS_OP_WRITE, "a.txt", {"data": [104, 105], "append": False, "mode": None}
        )
        self.assertEqual(written["bytes_written"], 2)

    def test_fs_error_frame_raises_remote_error(self):
        self.server.fail_fs_message = "path escapes allowed root"
        with self.assertRaises(RemoteError) as ctx:
            self.client.fs_op(1, "../etc", {"max_results": 1000})
        self.assertIn("path escapes allowed root", str(ctx.exception))

    def test_connection_refused_is_transport_error(self):
        dead = z._ZbrtGuestClient(f"127.0.0.1:{_closed_port()}", 2.0)
        with self.assertRaises(TransportError):
            dead.ping()


class ZbrtStreamTests(unittest.TestCase):
    def setUp(self):
        self.server = FakeZbrtServer()
        self.server.start()
        self.addCleanup(self.server.stop)
        self.client = z._ZbrtGuestClient(self.server.address, 5.0)

    def test_stream_output_then_exit_then_none(self):
        stream = self.client.open_stream(["echo"], None)
        # ZBRT has no started frame: the first poll synthesizes Started
        # (Rust baseline client/zbrt.rs ZbrtStream::next_event).
        started = stream.next_event()
        self.assertEqual(started.kind.value, "started")
        self.assertEqual(started.data, b"")
        self.assertIsNone(started.code)
        stdout = stream.next_event()
        self.assertEqual(stdout.kind.value, "stdout")
        self.assertEqual(stdout.data, b"hello\n")
        exit_event = stream.next_event()
        self.assertEqual(exit_event.kind.value, "exit")
        self.assertEqual(exit_event.code, 0)
        self.assertIsNone(stream.next_event())
        self.assertIsNone(stream.next_event())

    def test_stream_stop_receives_cancel_ack(self):
        self.server.hold_execute_open = True
        stream = self.client.open_stream(["tail"], None)
        stream.stop()
        # The fake server answered Cancel with an empty-payload CancelAck and
        # then an Exit(-1) terminal for the held request.
        self.assertEqual(len(self.server.received_cancels), 1)
        # The cached Exit is delivered after the synthesized started event.
        started = stream.next_event()
        self.assertEqual(started.kind.value, "started")
        exit_event = stream.next_event()
        self.assertEqual(exit_event.kind.value, "exit")
        self.assertEqual(exit_event.code, -1)
        self.assertIsNone(stream.next_event())
        # stop is idempotent: no second Cancel goes on the wire.
        stream.stop()
        self.assertEqual(len(self.server.received_cancels), 1)

    def test_stream_stop_with_exit_before_cancel_ack_caches_exit(self):
        # Exit arrives during the stop drain BEFORE the CancelAck: stop must
        # return immediately (not block until timeout), mark the stream
        # terminal and cache the Exit for the next next_event.
        self.server.hold_execute_open = True
        self.server.exit_before_cancel_ack = True
        stream = self.client.open_stream(["tail"], None)
        stream.stop()
        self.assertEqual(len(self.server.received_cancels), 1)
        started = stream.next_event()
        self.assertEqual(started.kind.value, "started")
        exit_event = stream.next_event()
        self.assertEqual(exit_event.kind.value, "exit")
        self.assertEqual(exit_event.code, -1)
        self.assertIsNone(stream.next_event())

    def test_stream_stop_after_terminal_is_noop(self):
        stream = self.client.open_stream(["echo"], None)
        while stream.next_event() is not None:
            pass
        stream.stop()
        self.assertEqual(len(self.server.received_cancels), 0)

    def test_stream_send_input_unsupported_over_zbrt(self):
        stream = self.client.open_stream(["tail"], None)
        with self.assertRaises(RemoteError):
            stream.send_input("x")

    def test_stream_error_frame_raises_remote_error(self):
        self.server.fail_execute_message = "a turn is already active"
        stream = self.client.open_stream(["tail"], None)
        self.assertEqual(stream.next_event().kind.value, "started")
        with self.assertRaises(RemoteError):
            stream.next_event()
        # After the error the stream is terminal: next_event returns None and
        # stop() is a no-op.
        self.assertIsNone(stream.next_event())
        stream.stop()


class ZbrtHelloHandshakeTests(unittest.TestCase):
    """Every guest ZBRT connection is Hello-first (cross-language contract):
    the guest rejects all frames from an un-helloed connection, so the SDK
    must send Hello before Execute/Health/Fs on each fresh connection and
    validate the echoing HelloAck."""

    def setUp(self):
        self.server = FakeZbrtServer()
        self.server.start()
        self.addCleanup(self.server.stop)
        self.client = z._ZbrtGuestClient(self.server.address, 5.0)

    def test_hello_is_first_frame_on_every_connection(self):
        self.assertTrue(self.client.ping())
        self.client.exec(["echo", "hi"], "/", 5, b"")
        self.client.fs_op(z.FS_OP_LS, ".", {"max_results": 10})
        stream = self.client.open_stream(["echo"], None)
        self.assertEqual(stream.next_event().kind.value, "started")
        self.assertEqual(stream.next_event().kind.value, "stdout")
        self.assertEqual(stream.next_event().kind.value, "exit")
        # 统一温池契约：每个连接的首帧必是 Hello；池借出时重发 Hello 验活
        # （fake 服务器逐请求关连接 → 验活失败回退新连接，首帧仍是 Hello），
        # 所以 Hello 总数 ≥ 连接数，且每个连接至少 Hello 一次。
        kinds = self.server.received_first_frame_kinds
        self.assertTrue(kinds, "no connections observed")
        self.assertTrue(all(k == 1 for k in kinds),
                        f"non-Hello first frame: {kinds}")
        self.assertGreaterEqual(len(self.server.received_hellos), len(kinds))
        for client_name, caps in self.server.received_hellos:
            self.assertEqual(client_name, "rfb-sdk-python")
            self.assertEqual(caps, z.ZBRT_V1_CAPABILITIES)

    def test_hello_rejection_close_is_transport_error(self):
        self.server.reject_hello = "close"
        with self.assertRaises(TransportError):
            self.client.ping()

    def test_hello_error_frame_is_transport_error(self):
        self.server.reject_hello = "error"
        with self.assertRaises(TransportError) as ctx:
            self.client.ping()
        self.assertIn("hello", str(ctx.exception))

    def test_hello_wrong_reply_kind_is_transport_error(self):
        self.server.reject_hello = "wrong_kind"
        with self.assertRaises(TransportError):
            self.client.ping()

    def test_hello_malformed_error_payload_is_transport_error(self):
        # A truncated Error payload during the handshake is still a handshake
        # failure: every failed handshake maps to the Transport class, never
        # a raw DecodeError.
        self.server.reject_hello = "malformed_error"
        with self.assertRaises(TransportError):
            self.client.ping()
        self.assertEqual(self.server.received_executes, [])

    def test_hello_is_the_first_frame_written_before_any_read(self):
        # Mock-level ordering contract (independent of the fake server): the
        # FIRST frame written on a fresh socket is Hello with the client name
        # and the full V1 capability set, and the HelloAck round-trip happens
        # before the business request (Health here) goes out.
        sock = mock.Mock()
        written = []
        state = {"rid": None, "phase": "hello"}

        def fake_write(_sock, kind, request_id, payload=b""):
            written.append((kind, request_id, payload))
            state["rid"] = request_id

        def fake_read(_sock):
            if state["phase"] == "hello":
                assert written and written[0][0] == z.KIND_HELLO, (
                    "Hello must be written before any reply is read"
                )
                state["phase"] = "health"
                return (
                    z.KIND_HELLO_ACK,
                    state["rid"],
                    z.encode_hello_ack("rfb-zeroboot-guest", z.ZBRT_V1_CAPABILITIES),
                )
            return (z.KIND_HEALTH_ACK, state["rid"], z.encode_health(True, None))

        with mock.patch.object(z.socket, "create_connection", return_value=sock), \
             mock.patch.object(z, "write_frame", side_effect=fake_write), \
             mock.patch.object(z, "read_frame", side_effect=fake_read):
            self.assertTrue(self.client.ping())

        self.assertEqual(
            [kind for kind, _rid, _payload in written],
            [z.KIND_HELLO, z.KIND_HEALTH],
        )
        client_name, caps = z.decode_hello(written[0][2])
        self.assertEqual(client_name, "rfb-sdk-python")
        self.assertEqual(caps, z.ZBRT_V1_CAPABILITIES)

    def test_handshake_failure_means_no_request_frames(self):
        # A rejected handshake must abort before the real request goes out.
        self.server.reject_hello = "close"
        with self.assertRaises(TransportError):
            self.client.exec(["echo"], "/", 5, b"")
        self.assertEqual(self.server.received_executes, [])


class ZbrtExecOutputCapTests(unittest.TestCase):
    """One exec turn aggregates at most 16 MiB of Output (Rust baseline
    client/zbrt.rs MAX_EXEC_BYTES); exceeding it is a Remote-class error."""

    def setUp(self):
        self.server = FakeZbrtServer()
        self.server.start()
        self.addCleanup(self.server.stop)
        self.client = z._ZbrtGuestClient(self.server.address, 5.0)

    def test_output_over_limit_is_remote_error(self):
        self.server.exec_oversize_chunks = [5, 5]
        with mock.patch.object(z, "MAX_EXEC_BYTES", 8):
            with self.assertRaises(RemoteError) as ctx:
                self.client.exec(["echo"], "/", 5, b"")
        self.assertIn("output exceeded", str(ctx.exception))

    def test_output_exactly_at_limit_is_allowed(self):
        self.server.exec_oversize_chunks = [5, 3]
        with mock.patch.object(z, "MAX_EXEC_BYTES", 8):
            code, stdout, _stderr = self.client.exec(["echo"], "/", 5, b"")
        self.assertEqual(code, 0)
        self.assertEqual(stdout, b"\x00" * 8)

    def test_default_cap_is_16mib(self):
        self.assertEqual(z.MAX_EXEC_BYTES, 16 * 1024 * 1024)


class ZbrtServerFrameChecks(unittest.TestCase):
    """Sanity-check that the SDK puts well-formed frames on the wire."""

    def test_client_frames_are_canonical(self):
        server = FakeZbrtServer()
        server.start()
        self.addCleanup(server.stop)
        client = z._ZbrtGuestClient(server.address, 5.0)
        client.ping()
        # The fake server would have asserted on any malformed inbound frame
        # (its read_frame asserts magic and exact framing), so reaching this
        # point means the Health frame was canonical.


if __name__ == "__main__":
    unittest.main()
