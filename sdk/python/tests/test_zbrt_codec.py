"""Golden wire vectors and strict-decode rejection tests for the internal
ZBRT codec (rfb_sdk._zbrt).

The vectors live in ``sdk/shared/conformance/zbrt_vectors.json`` and are
shared byte-for-byte across all four SDKs; this file loads them from disk
(found by walking up from the test file to the repository root) and asserts
the codec reproduces the exact bytes in BOTH directions. The semantic
assertions (decoded field values) stay in code below.
"""

import json
import unittest
from pathlib import Path

from rfb_sdk import _zbrt as z

# Load the shared vectors by walking up from this file until an ancestor
# contains sdk/shared/conformance (works in-tree and from any cwd; the file is
# never copied into the language tree).
def _find_vectors_path() -> Path:
    relative = Path("sdk") / "shared" / "conformance"
    for ancestor in Path(__file__).resolve().parents:
        candidate = ancestor / relative
        if candidate.is_dir():
            return candidate / "zbrt_vectors.json"
    raise FileNotFoundError(
        "sdk/shared/conformance/zbrt_vectors.json not found above "
        f"{Path(__file__).resolve()}"
    )


VECTORS_PATH = _find_vectors_path()

with VECTORS_PATH.open("r", encoding="utf-8") as _fh:
    VECTORS = json.load(_fh)

RID = bytes.fromhex(VECTORS["request_id_hex"])
FRAMES = {frame["name"]: frame for frame in VECTORS["frames"]}
REJECTS = VECTORS["rejects"]

EXPECTED_FRAME_NAMES = {
    "hello",
    "helloack",
    "execute",
    "output",
    "exit",
    "cancel",
    "cancel_legacy",
    "error",
}

# name -> (payload encoder, payload decoder, expected decoded value)
# The expected decode values ARE the semantic contract of each vector frame;
# they also feed the encode direction so encode/decode must both hit the
# shared bytes.
SEMANTICS = {
    "hello": (
        lambda: z.encode_hello("sdk-test", ["execute", "stream"]),
        z.decode_hello,
        ("sdk-test", ["execute", "stream"]),
    ),
    "helloack": (
        lambda: z.encode_hello_ack("rfb-zeroboot-guest", z.ZBRT_V1_CAPABILITIES),
        z.decode_hello_ack,
        ("rfb-zeroboot-guest", z.ZBRT_V1_CAPABILITIES),
    ),
    "execute": (
        lambda: z.encode_execute(["echo", "hi"], "/workspace", b"abc", 1500),
        z.decode_execute,
        (["echo", "hi"], "/workspace", b"abc", 1500),
    ),
    "output": (
        lambda: z.encode_output(1, b"err line\n"),
        z.decode_output,
        (1, b"err line\n"),
    ),
    "exit": (
        lambda: z.encode_exit(0, None),
        z.decode_exit,
        (0, None),
    ),
    "cancel": (
        lambda: z.encode_cancel("user", None),  # modern form: explicit 0 target flag
        z.decode_cancel,
        ("user", None),
    ),
    # Legacy payloads end right after the reason; target decodes to None.
    "cancel_legacy": (
        lambda: z.encode_cancel("user", None, include_target_field=False),
        z.decode_cancel,
        ("user", None),
    ),
    "error": (
        lambda: z.encode_error(1, "argv is empty"),
        z.decode_error,
        (1, "argv is empty"),
    ),
}


class GoldenVectorTests(unittest.TestCase):
    """Every shared vector must hit on encode AND decode, byte for byte."""

    def test_vector_file_has_the_expected_shape(self):
        self.assertEqual(set(FRAMES), EXPECTED_FRAME_NAMES)
        self.assertEqual(len(RID), 16)

    def test_all_frames_encode_and_decode_to_shared_bytes(self):
        for name in sorted(EXPECTED_FRAME_NAMES):
            with self.subTest(vector=name):
                frame = FRAMES[name]
                kind = frame["kind"]
                golden = frame["hex"]
                encoder, decoder, expected = SEMANTICS[name]

                # Encode direction: canonical payload + frame header.
                payload = encoder()
                self.assertEqual(payload.hex(), golden[56:])
                self.assertEqual(
                    z.encode_frame(kind, RID, payload).hex(), golden
                )

                # Decode direction: strict frame split + semantic fields.
                decoded_kind, decoded_rid, decoded_payload = z.decode_frame(
                    bytes.fromhex(golden)
                )
                self.assertEqual(decoded_kind, kind)
                self.assertEqual(decoded_rid, RID)
                self.assertEqual(decoder(decoded_payload), expected)

    def test_helloack_carries_the_full_v1_capability_set(self):
        # Semantic assertion kept in code (mirrors the Rust golden test):
        # exactly six capabilities in canonical order.
        # 28-byte frame header = 56 hex chars of payload offset.
        _server, caps = z.decode_hello_ack(bytes.fromhex(FRAMES["helloack"]["hex"])[28:])
        self.assertEqual(len(caps), 6)
        self.assertEqual(caps[0], "execute")
        self.assertEqual(caps[-1], "filesystem")
        self.assertEqual(caps, z.ZBRT_V1_CAPABILITIES)


class SharedRejectVectorTests(unittest.TestCase):
    """The shared ``rejects`` entries mutate a good frame at a pinned byte
    offset and must fail strict decode."""

    def test_reject_vectors_fail_to_decode(self):
        good = bytes.fromhex(FRAMES["hello"]["hex"])
        self.assertTrue(REJECTS, "reject vectors must be present")
        for reject in REJECTS:
            with self.subTest(reject=reject["name"]):
                mutated = bytearray(good)
                mutated[reject["byte_offset"]] = reject["byte_value"]
                with self.assertRaises(z.DecodeError):
                    z.decode_frame(bytes(mutated))


class StrictDecodeRejectionTests(unittest.TestCase):
    """Codec-level rejection edges beyond the shared vector file."""

    def _frame(self, *, magic=z.MAGIC, version=z.VERSION, kind=1, flags=0, payload_len=None, payload=b""):
        if payload_len is None:
            payload_len = len(payload)
        header = magic + struct_pack(version, kind, flags) + RID + pack_u32(payload_len)
        return header + payload

    def test_nonzero_flags(self):
        with self.assertRaises(Exception) as ctx:
            z.decode_frame(self._frame(flags=1, payload=b""))
        self.assertIn("flags", str(ctx.exception))

    def test_truncated_header(self):
        with self.assertRaises(Exception) as ctx:
            z.decode_frame(b"ZBRT\x01\x01\x00")
        self.assertIn("header", str(ctx.exception))

    def test_truncated_payload(self):
        with self.assertRaises(Exception) as ctx:
            z.decode_frame(self._frame(kind=z.KIND_EXIT, payload_len=5, payload=b"\x00\x00"))
        self.assertIn("truncated", str(ctx.exception))

    def test_trailing_bytes_after_frame(self):
        good = z.encode_frame(z.KIND_CANCEL_ACK, RID, b"")
        with self.assertRaises(Exception) as ctx:
            z.decode_frame(good + b"\x00")
        self.assertIn("trailing", str(ctx.exception))

    def test_oversize_payload_len(self):
        header = z.MAGIC + struct_pack(1, z.KIND_HELLO, 0) + RID + pack_u32(z.MAX_PAYLOAD + 1)
        with self.assertRaises(Exception) as ctx:
            z.decode_frame(header)
        self.assertIn("payload too large", str(ctx.exception))

    def test_payload_trailing_bytes(self):
        payload = z.encode_hello_ack("srv", ["execute"]) + b"\x00"
        with self.assertRaises(Exception) as ctx:
            z.decode_hello_ack(payload)
        self.assertIn("trailing", str(ctx.exception))

    def test_payload_truncated_string(self):
        payload = z.encode_hello("sdk-test", ["execute", "stream"])
        with self.assertRaises(Exception):
            z.decode_hello(payload[:-1])
        with self.assertRaises(Exception):
            z.decode_hello(payload[:8])

    def test_payload_truncated_execute(self):
        payload = z.encode_execute(["echo", "hi"], "/workspace", b"abc", 1500)
        for cut in (1, 5, len(payload) - 4):
            with self.assertRaises(Exception):
                z.decode_execute(payload[:cut])

    def test_payload_invalid_utf8(self):
        with self.assertRaises(Exception):
            z.decode_hello(b"\x00\x00\x00\x02\xff\xfe\x01\x00\x00\x00\x01a")

    def test_encode_rejects_oversize_payload(self):
        with self.assertRaises(ValueError):
            z.encode_frame(z.KIND_EXECUTE, RID, b"\x00" * (z.MAX_PAYLOAD + 1))

    def test_encode_rejects_bad_request_id(self):
        with self.assertRaises(ValueError):
            z.encode_frame(z.KIND_HELLO, b"short", b"")


def struct_pack(version, kind, flags):
    import struct

    return struct.pack(">BBH", version, kind, flags)


def pack_u32(value):
    import struct

    return struct.pack(">I", value)


if __name__ == "__main__":
    unittest.main()
