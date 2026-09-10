"""Golden vectors (PROTOCOL.md section 4) and strict-decode rejection tests
for the internal ZBRT codec (rfb_sdk._zbrt)."""

import unittest

from rfb_sdk import _zbrt as z

RID = bytes(range(16))
RID_HEX = RID.hex()

# name -> (full frame hex, kind, payload-bytes builder)
GOLDEN_FRAMES = {
    "HELLO": (
        "5a42525401010000000102030405060708090a0b0c0d0e0f000000220000000873646b2d746573740200000007657865637574650000000673747265616d",
        z.KIND_HELLO,
    ),
    "HELLOACK": (
        "5a42525401020000000102030405060708090a0b0c0d0e0f0000005a000000127266622d7a65726f626f6f742d67756573740600000007657865637574650000000673747265616d00000008646561646c696e65000000066865616c74680000000663616e63656c0000000a66696c6573797374656d",
        z.KIND_HELLO_ACK,
    ),
    "EXECUTE": (
        "5a42525401030000000102030405060708090a0b0c0d0e0f0000002902000000046563686f000000026869010000000a2f776f726b737061636500000003616263000005dc",
        z.KIND_EXECUTE,
    ),
    "OUTPUT": (
        "5a42525401040000000102030405060708090a0b0c0d0e0f0000000e0100000009657272206c696e650a",
        z.KIND_OUTPUT,
    ),
    "EXIT": (
        "5a42525401050000000102030405060708090a0b0c0d0e0f000000050000000000",
        z.KIND_EXIT,
    ),
    "CANCEL": (
        "5a42525401060000000102030405060708090a0b0c0d0e0f0000000a01000000047573657200",
        z.KIND_CANCEL,
    ),
    "CANCEL_LEGACY": (
        "5a42525401060000000102030405060708090a0b0c0d0e0f00000009010000000475736572",
        z.KIND_CANCEL,
    ),
    "ERROR": (
        "5a425254010c0000000102030405060708090a0b0c0d0e0f00000015000000010000000d6172677620697320656d707479",
        z.KIND_ERROR,
    ),
}


def frame_hex_to_parts(hex_text):
    return z.decode_frame(bytes.fromhex(hex_text))


class GoldenVectorTests(unittest.TestCase):
    """Each of the 8 golden vectors must hit on encode AND decode."""

    def test_hello_encode_decode(self):
        payload = z.encode_hello("sdk-test", ["execute", "stream"])
        self.assertEqual(payload.hex(), GOLDEN_FRAMES["HELLO"][0][56:])
        self.assertEqual(
            z.encode_frame(z.KIND_HELLO, RID, payload).hex(), GOLDEN_FRAMES["HELLO"][0]
        )
        kind, rid, payload_back = frame_hex_to_parts(GOLDEN_FRAMES["HELLO"][0])
        self.assertEqual((kind, rid), (z.KIND_HELLO, RID))
        self.assertEqual(z.decode_hello(payload_back), ("sdk-test", ["execute", "stream"]))

    def test_helloack_encode_decode(self):
        caps = ["execute", "stream", "deadline", "health", "cancel", "filesystem"]
        payload = z.encode_hello_ack("rfb-zeroboot-guest", caps)
        self.assertEqual(payload.hex(), GOLDEN_FRAMES["HELLOACK"][0][56:])
        self.assertEqual(
            z.encode_frame(z.KIND_HELLO_ACK, RID, payload).hex(), GOLDEN_FRAMES["HELLOACK"][0]
        )
        kind, rid, payload_back = frame_hex_to_parts(GOLDEN_FRAMES["HELLOACK"][0])
        self.assertEqual((kind, rid), (z.KIND_HELLO_ACK, RID))
        self.assertEqual(z.decode_hello_ack(payload_back), ("rfb-zeroboot-guest", caps))

    def test_execute_encode_decode(self):
        payload = z.encode_execute(["echo", "hi"], "/workspace", b"abc", 1500)
        self.assertEqual(payload.hex(), GOLDEN_FRAMES["EXECUTE"][0][56:])
        self.assertEqual(
            z.encode_frame(z.KIND_EXECUTE, RID, payload).hex(), GOLDEN_FRAMES["EXECUTE"][0]
        )
        kind, rid, payload_back = frame_hex_to_parts(GOLDEN_FRAMES["EXECUTE"][0])
        self.assertEqual((kind, rid), (z.KIND_EXECUTE, RID))
        self.assertEqual(
            z.decode_execute(payload_back), (["echo", "hi"], "/workspace", b"abc", 1500)
        )

    def test_output_encode_decode(self):
        payload = z.encode_output(1, b"err line\n")
        self.assertEqual(payload.hex(), GOLDEN_FRAMES["OUTPUT"][0][56:])
        self.assertEqual(
            z.encode_frame(z.KIND_OUTPUT, RID, payload).hex(), GOLDEN_FRAMES["OUTPUT"][0]
        )
        kind, rid, payload_back = frame_hex_to_parts(GOLDEN_FRAMES["OUTPUT"][0])
        self.assertEqual((kind, rid), (z.KIND_OUTPUT, RID))
        self.assertEqual(z.decode_output(payload_back), (1, b"err line\n"))

    def test_exit_encode_decode(self):
        payload = z.encode_exit(0, None)
        self.assertEqual(payload.hex(), GOLDEN_FRAMES["EXIT"][0][56:])
        self.assertEqual(
            z.encode_frame(z.KIND_EXIT, RID, payload).hex(), GOLDEN_FRAMES["EXIT"][0]
        )
        kind, rid, payload_back = frame_hex_to_parts(GOLDEN_FRAMES["EXIT"][0])
        self.assertEqual((kind, rid), (z.KIND_EXIT, RID))
        self.assertEqual(z.decode_exit(payload_back), (0, None))

    def test_cancel_encode_decode(self):
        payload = z.encode_cancel("user", None)  # modern form: explicit 0 target flag
        self.assertEqual(payload.hex(), GOLDEN_FRAMES["CANCEL"][0][56:])
        self.assertEqual(
            z.encode_frame(z.KIND_CANCEL, RID, payload).hex(), GOLDEN_FRAMES["CANCEL"][0]
        )
        kind, rid, payload_back = frame_hex_to_parts(GOLDEN_FRAMES["CANCEL"][0])
        self.assertEqual((kind, rid), (z.KIND_CANCEL, RID))
        self.assertEqual(z.decode_cancel(payload_back), ("user", None))

    def test_cancel_legacy_encode_decode(self):
        payload = z.encode_cancel("user", None, include_target_field=False)
        self.assertEqual(payload.hex(), GOLDEN_FRAMES["CANCEL_LEGACY"][0][56:])
        self.assertEqual(
            z.encode_frame(z.KIND_CANCEL, RID, payload).hex(), GOLDEN_FRAMES["CANCEL_LEGACY"][0]
        )
        kind, rid, payload_back = frame_hex_to_parts(GOLDEN_FRAMES["CANCEL_LEGACY"][0])
        self.assertEqual((kind, rid), (z.KIND_CANCEL, RID))
        # Legacy payload (no target byte) decodes to target=None.
        self.assertEqual(z.decode_cancel(payload_back), ("user", None))

    def test_error_encode_decode(self):
        payload = z.encode_error(1, "argv is empty")
        self.assertEqual(payload.hex(), GOLDEN_FRAMES["ERROR"][0][56:])
        self.assertEqual(
            z.encode_frame(z.KIND_ERROR, RID, payload).hex(), GOLDEN_FRAMES["ERROR"][0]
        )
        kind, rid, payload_back = frame_hex_to_parts(GOLDEN_FRAMES["ERROR"][0])
        self.assertEqual((kind, rid), (z.KIND_ERROR, RID))
        self.assertEqual(z.decode_error(payload_back), (1, "argv is empty"))


class StrictDecodeRejectionTests(unittest.TestCase):
    def _frame(self, *, magic=z.MAGIC, version=z.VERSION, kind=1, flags=0, payload_len=None, payload=b""):
        if payload_len is None:
            payload_len = len(payload)
        header = magic + struct_pack(version, kind, flags) + RID + pack_u32(payload_len)
        return header + payload

    def test_bad_magic(self):
        with self.assertRaises(Exception) as ctx:
            z.decode_frame(self._frame(magic=b"XRRT", payload=b""))
        self.assertIn("magic", str(ctx.exception))

    def test_wrong_version(self):
        with self.assertRaises(Exception) as ctx:
            z.decode_frame(self._frame(version=2, payload=b""))
        self.assertIn("version", str(ctx.exception))

    def test_nonzero_flags(self):
        with self.assertRaises(Exception) as ctx:
            z.decode_frame(self._frame(flags=1, payload=b""))
        self.assertIn("flags", str(ctx.exception))

    def test_unknown_kind(self):
        for kind in (0, 14, 200):
            with self.assertRaises(Exception) as ctx:
                z.decode_frame(self._frame(kind=kind, payload=b""))
            self.assertIn("kind", str(ctx.exception))

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
