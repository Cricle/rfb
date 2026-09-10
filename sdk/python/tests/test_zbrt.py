"""Shared eval-over-ZBRT conformance vectors.

Source of truth: sdk/shared/conformance/eval_zbrt_vectors.json (baseline:
Rust ``rfb::client``). The test name matches the JSON's
``per_language_test_names.python`` entry
(``tests/test_zbrt.py::test_eval_zbrt_matches_shared_vector``).
"""

import io
import json
import pathlib
import struct
import unittest

from rfb_sdk import RfbClient
from rfb_sdk import _zbrt as z
from rfb_sdk.errors import ValidationError

from tests.fake_servers import FakeControllerServer, FakeZbrtServer

VECTOR_PATH = (
    pathlib.Path(__file__).resolve().parents[2]
    / "shared"
    / "conformance"
    / "eval_zbrt_vectors.json"
)
GOLDEN_RID = bytes.fromhex("000102030405060708090a0b0c0d0e0f")


# -- hand-rolled frame helpers (independent of the SDK codec) -----------------


def _frame(kind: int, request_id: bytes, payload: bytes) -> bytes:
    return struct.pack(">4sBBH16sI", b"ZBRT", 1, kind, 0, request_id, len(payload)) + payload


def _output_frame(request_id: bytes, stream: int, data: bytes) -> bytes:
    return _frame(4, request_id, bytes([stream]) + struct.pack(">I", len(data)) + data)


def _exit_frame(request_id: bytes, code: int) -> bytes:
    return _frame(5, request_id, struct.pack(">i", code) + b"\x00")


class _FakeSocket:
    """Records written frames; replays canned reply bytes on recv."""

    def __init__(self, reply_bytes: bytes):
        self.sent = bytearray()
        self._rbuf = io.BytesIO(reply_bytes)

    def setsockopt(self, *args, **kwargs):
        pass

    def sendall(self, data, *args):
        self.sent += bytes(data)

    def recv(self, n, *args):
        return self._rbuf.read(n) or b""

    def close(self):
        pass


class EvalZbrtConformanceTests(unittest.TestCase):
    """Golden vectors from sdk/shared/conformance/eval_zbrt_vectors.json."""

    def setUp(self):
        self.vectors = json.loads(VECTOR_PATH.read_text(encoding="utf-8"))
        original_rid = z.new_request_id
        z.new_request_id = lambda: GOLDEN_RID
        self.addCleanup(setattr, z, "new_request_id", original_rid)

    # -- helpers -------------------------------------------------------------

    def _eval_wire(self, code, cwd, timeout_s, reply_bytes):
        """Run _ZbrtGuestClient.eval over a fake socket; -> (result, sent bytes)."""
        sock = _FakeSocket(reply_bytes)
        client = z._ZbrtGuestClient("127.0.0.1:1", 5.0)
        client._connect = lambda _timeout: sock
        return client.eval(code, cwd, timeout_s), bytes(sock.sent)

    def _zbrt_sandbox(self, guest):
        controller = FakeControllerServer(
            snapshots=[{"tag": "base", "status": "ready", "bootable": True}],
            snapshot_by_tag={"base": {"tag": "base", "status": "ready", "bootable": True}},
            guest_addr=guest.address,
        )
        controller.start()
        self.addCleanup(controller.stop)
        client = RfbClient(base_url=controller.url, timeout_s=5.0)
        return client.create_sandbox("base", transport="zbrt")[0]

    # -- the shared conformance test -----------------------------------------

    def test_eval_zbrt_matches_shared_vector(self):
        for vector in self.vectors["vectors"]:
            with self.subTest(name=vector["name"]):
                if "expected_frame_hex" in vector:
                    self._assert_encode_vector(vector)
                else:
                    self._assert_rejection_vector(vector)

    def _assert_encode_vector(self, vector):
        inp = vector["input"]
        rid = GOLDEN_RID
        reply = b""
        for frame in vector["expected_response_mapping_example"]["frames"]:
            if frame["kind"] == 4:
                reply += _output_frame(rid, frame["stream"], bytes.fromhex(frame["data_hex"]))
            elif frame["kind"] == 5:
                reply += _exit_frame(rid, frame["exit_code"])
            else:
                raise AssertionError(f"unexpected response frame kind {frame['kind']}")
        result, sent = self._eval_wire(inp["code"], inp["cwd"], inp["timeout_s"], reply)

        # Exactly one Execute frame, byte-identical to the golden vector.
        self.assertEqual(sent.hex(), vector["expected_frame_hex"])
        kind, request_id, payload = z.decode_frame(sent)
        self.assertEqual(kind, 3)
        self.assertEqual(request_id, GOLDEN_RID)

        # Field-level assertions for precise failure diagnosis.
        expected = vector["expected_execute"]
        argv, cwd, stdin, timeout_ms = z.decode_execute(payload)
        self.assertEqual(argv, expected["argv"])
        self.assertEqual(cwd, expected["cwd"])
        self.assertEqual(stdin, bytes.fromhex(expected["stdin_hex"]))
        self.assertEqual(timeout_ms, expected["timeout_ms"])

        # Result mapping: Output(stream=0) -> stdout, Exit -> exit_code,
        # timed_out always False.
        expected_result = vector["expected_response_mapping_example"]["exec_result"]
        code, stdout, stderr = result
        self.assertEqual(code, expected_result["exit_code"])
        self.assertEqual(stdout, bytes.fromhex(expected_result["stdout_hex"]))
        self.assertEqual(stderr, bytes.fromhex(expected_result["stderr_hex"]))

    def _assert_rejection_vector(self, vector):
        guest = FakeZbrtServer()
        guest.start()
        self.addCleanup(guest.stop)
        sandbox = self._zbrt_sandbox(guest)
        inp = vector["input"]
        with self.assertRaises(ValidationError):
            sandbox.eval(inp["code"], cwd=inp["cwd"], timeout_s=inp["timeout_s"])
        self.assertEqual(len(guest.received_executes), 0, "no frames sent")
        self.assertEqual(guest.connections, 0, "no connections opened")


class EvalZbrtFacadeTests(unittest.TestCase):
    """Facade-level eval-over-ZBRT regression (argv convention + result shape)."""

    def test_eval_sends_eval_op_argv_over_zbrt(self):
        guest = FakeZbrtServer()
        guest.start()
        self.addCleanup(guest.stop)
        controller = FakeControllerServer(
            snapshots=[{"tag": "base", "status": "ready", "bootable": True}],
            snapshot_by_tag={"base": {"tag": "base", "status": "ready", "bootable": True}},
            guest_addr=guest.address,
        )
        controller.start()
        self.addCleanup(controller.stop)
        client = RfbClient(base_url=controller.url, timeout_s=5.0)
        sandbox = client.create_sandbox("base", transport="zbrt")[0]

        result = sandbox.eval("print(40+2)")

        argv, cwd, stdin, timeout_ms = guest.received_executes[0]
        self.assertEqual(argv, ["eval", "print(40+2)"])
        self.assertIsNone(cwd)
        self.assertEqual(stdin, b"")
        self.assertEqual(timeout_ms, 0)
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(result.stdout, b"eval out\n")
        self.assertEqual(result.stderr, b"")
        self.assertFalse(result.timed_out)


if __name__ == "__main__":
    unittest.main()
