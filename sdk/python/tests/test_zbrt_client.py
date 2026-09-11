"""ZBRT guest client tests against a fake frame server (rfb_sdk._zbrt)."""

import socket
import struct
import unittest

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

    def test_eval_maps_to_eval_op_argv(self):
        _code, stdout, _stderr = self.client.eval("print(1)", None, None)
        self.assertEqual(stdout, b"eval out\n")
        argv, cwd, _stdin, timeout_ms = self.server.received_executes[0]
        # Shared contract (sdk/shared/README.md §1): argv=["eval", code].
        self.assertEqual(argv, ["eval", "print(1)"])
        self.assertIsNone(cwd)
        self.assertEqual(timeout_ms, 0)

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
        stdout = stream.next_event()
        self.assertEqual(stdout.kind.value, "stdout")
        self.assertEqual(stdout.data, b"hello\n")
        exit_event = stream.next_event()
        self.assertEqual(exit_event.kind.value, "exit")
        self.assertEqual(exit_event.code, 0)
        self.assertIsNone(stream.next_event())

    def test_stream_stop_receives_cancel_ack(self):
        self.server.hold_execute_open = True
        stream = self.client.open_stream(["tail"], None)
        stream.stop()
        # The fake server answered Cancel with an empty-payload CancelAck and
        # then an Exit(-1) terminal for the held request.
        self.assertEqual(len(self.server.received_cancels), 1)
        exit_event = stream.next_event()
        self.assertEqual(exit_event.kind.value, "exit")
        self.assertEqual(exit_event.code, -1)
        self.assertIsNone(stream.next_event())
        # stop is idempotent: no second Cancel goes on the wire.
        stream.stop()
        self.assertEqual(len(self.server.received_cancels), 1)

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
        with self.assertRaises(RemoteError):
            stream.next_event()
        # After the error the stream is terminal: next_event returns None and
        # stop() is a no-op.
        self.assertIsNone(stream.next_event())
        stream.stop()


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
