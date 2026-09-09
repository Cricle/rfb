"""forkd guest NDJSON adapter tests against a fake TCP server (rfb_sdk._guest)."""

import socket
import unittest

from rfb_sdk import _guest
from rfb_sdk.errors import DecodeError, RemoteError, TransportError, ValidationError
from rfb_sdk.validation import validate_fs_path, validate_guest_file_path

from tests.fake_servers import FakeNdjsonGuestServer


def _closed_port():
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


class GuestNdjsonTests(unittest.TestCase):
    def setUp(self):
        self.server = FakeNdjsonGuestServer()
        self.server.start()
        self.addCleanup(self.server.stop)
        self.client = _guest._GuestNdjsonClient(self.server.address, 5.0)

    def test_exec_reads_lines_until_terminal(self):
        # The fake sends a non-terminal stdout line first, then the terminal
        # exit_code line; the client must keep reading until the terminal.
        value = self.client.exec("/", ["echo", "hi"], 10)
        self.assertEqual(value["exit_code"], 0)
        self.assertEqual(value["timed_out"], False)
        self.assertEqual(self.server.received[0]["action"], "exec")
        self.assertEqual(self.server.received[0]["cwd"], "/")
        self.assertEqual(self.server.received[0]["timeout"], 10)

    def test_exec_timeout_wire_value_coerced_to_int(self):
        self.client.exec("/", ["echo"], 60.0)
        self.assertEqual(self.server.received[0]["timeout"], 60)

    def test_exec_timeout_ceil_to_whole_seconds(self):
        # Rust baseline (validation::timeout_secs): whole seconds on the
        # wire — ceil with a minimum of 1, same as the eval path.
        self.client.exec("/", ["echo"], 1.4)
        self.assertEqual(self.server.received[0]["timeout"], 2)
        self.client.exec("/", ["echo"], 0.2)
        self.assertEqual(self.server.received[1]["timeout"], 1)

    def test_error_line_raises_remote_error(self):
        self.server.error_response = "nope"
        with self.assertRaises(RemoteError) as ctx:
            self.client.ping()
        self.assertIn("nope", str(ctx.exception))

    def test_oversize_line_raises_decode_error(self):
        self.server.oversize_response = True
        with self.assertRaises(DecodeError):
            self.client.ping()

    def test_ping(self):
        self.assertEqual(self.client.ping(), {"pong": True})

    def test_tool_roundtrips(self):
        self.assertEqual(self.client.tool({"action": "ls", "path": ".", "max_results": 1000})["entries"][0]["name"], "a.txt")
        self.assertEqual(self.client.tool({"action": "read", "path": "a.txt"})["data"], list(b"hi"))
        self.assertEqual(self.client.tool({"action": "write", "path": "a.txt", "data": [1, 2, 3], "append": False})["bytes_written"], 3)

    def test_validation_fails_closed_before_connecting(self):
        connections_before = self.server.connections
        with self.assertRaises(ValidationError):
            validate_fs_path("../escape")
        with self.assertRaises(ValidationError):
            validate_guest_file_path("C:/windows")  # drive letter is a host-ism
        with self.assertRaises(ValidationError):
            validate_fs_path("a\\b")  # backslash is rejected everywhere
        with self.assertRaises(ValidationError):
            validate_fs_path("x" * 4097)
        self.assertEqual(self.server.connections, connections_before)

    def test_connection_refused_is_transport_error(self):
        dead = _guest._GuestNdjsonClient(f"127.0.0.1:{_closed_port()}", 2.0)
        with self.assertRaises(TransportError):
            dead.ping()

    def test_invalid_address_is_decode_error(self):
        with self.assertRaises(DecodeError):
            _guest._GuestNdjsonClient("no-port-here", 5.0)


class GuestNdjsonStreamTests(unittest.TestCase):
    def setUp(self):
        self.server = FakeNdjsonGuestServer()
        self.server.start()
        self.addCleanup(self.server.stop)
        self.client = _guest._GuestNdjsonClient(self.server.address, 5.0)

    def test_stream_session_lifecycle(self):
        stream = self.client.stream(["cat"], None, None, None)
        started = stream.next_event()
        self.assertEqual(started.kind.value, "started")
        stream.send_input("abc\n")
        stdout = stream.next_event()
        self.assertEqual(stdout.kind.value, "stdout")
        self.assertEqual(stdout.data, b"abc\n")
        stream.stop()
        exit_event = stream.next_event()
        self.assertEqual(exit_event.kind.value, "exit")
        self.assertEqual(exit_event.code, 130)
        self.assertIsNone(stream.next_event())
        self.assertTrue(self.server.stop_requested)
        # send_input after terminal raises; stop is idempotent.
        with self.assertRaises(RemoteError):
            stream.send_input("late")
        stream.stop()

    def test_stream_stop_idempotent_before_terminal(self):
        stream = self.client.stream(["cat"], None, None, None)
        stream.next_event()  # started
        stream.stop()
        exit_event = stream.next_event()  # drain the guest's exit event
        self.assertEqual(exit_event.kind.value, "exit")
        stream.stop()  # second stop is a no-op
        self.assertEqual(
            sum(1 for v in self.server.stream_inputs if v.get("action") == "stop"), 1
        )

    def test_stream_send_input_after_stop_raises(self):
        stream = self.client.stream(["cat"], None, None, None)
        stream.next_event()
        stream.stop()
        with self.assertRaises(RemoteError):
            stream.send_input("x")


if __name__ == "__main__":
    unittest.main()
