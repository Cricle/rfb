"""forkd controller adapter tests against a fake HTTP server (rfb_sdk._forkd)."""

import json
import socket
import unittest

from rfb_sdk import _forkd
from rfb_sdk.errors import DecodeError, HttpStatusError, TransportError, ValidationError

from tests.fake_servers import FakeControllerServer


def _closed_port():
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


SNAPSHOT = {
    "tag": "base",
    "dir": "/var/rfb/base",
    "status": "ready",
    "bootable": True,
    "created_at_unix": 1700000000,
}


class ForkdControllerTests(unittest.TestCase):
    def setUp(self):
        self.server = FakeControllerServer(
            snapshots=[SNAPSHOT],
            snapshot_by_tag={
                "base": SNAPSHOT,
                "legacy-only": {"tag": "legacy-only", "status": "ready", "bootable": True},
            },
            info_404_tags={"legacy-only"},
            delete_404_ids={"gone"},
        )
        self.server.start()
        self.addCleanup(self.server.stop)
        self.controller = _forkd._ForkdController(self.server.url, "sekrit", 5.0)

    def test_list_snapshots(self):
        snapshots = self.controller.list_snapshots()
        self.assertEqual(len(snapshots), 1)
        self.assertEqual(snapshots[0]["tag"], "base")
        request = self.server.requests[-1]
        self.assertEqual(request["method"], "GET")
        self.assertEqual(request["path"], "/v1/snapshots")

    def test_bearer_header_sent_when_token_set(self):
        self.controller.list_snapshots()
        self.assertEqual(self.server.requests[-1]["headers"].get("Authorization"), "Bearer sekrit")

    def test_no_bearer_header_without_token(self):
        anonymous = _forkd._ForkdController(self.server.url, None, 5.0)
        anonymous.list_snapshots()
        self.assertNotIn("Authorization", self.server.requests[-1]["headers"])

    def test_snapshot_info_endpoint(self):
        value = self.controller.snapshot("base")
        self.assertEqual(value["tag"], "base")
        self.assertEqual(self.server.requests[-1]["path"], "/v1/snapshots/base/info")

    def test_snapshot_falls_back_to_legacy_endpoint(self):
        value = self.controller.snapshot("legacy-only")
        self.assertIsNotNone(value)
        paths = [r["path"] for r in self.server.requests[-2:]]
        self.assertEqual(
            paths, ["/v1/snapshots/legacy-only/info", "/v1/snapshots/legacy-only"]
        )

    def test_snapshot_both_404_returns_none(self):
        self.assertIsNone(self.controller.snapshot("missing"))

    def test_error_mapping_prefers_json_error_field(self):
        self.server.behavior = lambda m, p, b: (500, json.dumps({"error": "boom"}).encode())
        with self.assertRaises(HttpStatusError) as ctx:
            self.controller.list_snapshots()
        self.assertEqual(ctx.exception.status, 500)
        self.assertEqual(ctx.exception.message, "boom")

    def test_error_mapping_raw_body_prefix(self):
        raw = "x" * 2000
        self.server.behavior = lambda m, p, b: (503, raw.encode())
        with self.assertRaises(HttpStatusError) as ctx:
            self.controller.list_snapshots()
        self.assertEqual(ctx.exception.status, 503)
        self.assertEqual(ctx.exception.message, "x" * 1024)

    def test_create_sandboxes_posts_full_request(self):
        request = {
            "snapshot_tag": "base",
            "n": 2,
            "per_child_netns": False,
            "memory_limit_mib": None,
            "prewarm": False,
            "live_fork": False,
            "hugepages": False,
        }
        created = self.controller.create_sandboxes(request)
        self.assertEqual(len(created), 2)
        sent = json.loads(self.server.requests[-1]["body"])
        self.assertEqual(sent, request)

    def test_list_sandboxes(self):
        self.controller.create_sandboxes(
            {
                "snapshot_tag": "base",
                "n": 1,
                "per_child_netns": False,
                "memory_limit_mib": None,
                "prewarm": False,
                "live_fork": False,
                "hugepages": False,
            }
        )
        sandboxes = self.controller.list_sandboxes()
        self.assertEqual(sandboxes[0]["id"], "sb-1")

    def test_ping_sandbox(self):
        value = self.controller.ping_sandbox("sb-1")
        self.assertEqual(value, {"pong": True})
        self.assertEqual(self.server.requests[-1]["path"], "/v1/sandboxes/sb-1/ping")

    def test_ping_sandbox_rejects_bad_id(self):
        for bad in ("", "bad id", "x" * 129, "../etc"):
            with self.assertRaises(ValidationError):
                self.controller.ping_sandbox(bad)

    def test_delete_sandbox_2xx(self):
        self.controller.create_sandboxes(
            {
                "snapshot_tag": "base",
                "n": 1,
                "per_child_netns": False,
                "memory_limit_mib": None,
                "prewarm": False,
                "live_fork": False,
                "hugepages": False,
            }
        )
        self.assertIsNone(self.controller.delete_sandbox("sb-1"))

    def test_delete_sandbox_404_is_success(self):
        self.assertIsNone(self.controller.delete_sandbox("gone"))

    def test_delete_sandbox_other_error_raises(self):
        self.server.behavior = lambda m, p, b: (500, json.dumps({"error": "boom"}).encode())
        with self.assertRaises(HttpStatusError):
            self.controller.delete_sandbox("sb-1")

    def test_invalid_json_response_is_decode_error(self):
        self.server.behavior = lambda m, p, b: (200, b"not json")
        with self.assertRaises(DecodeError):
            self.controller.list_snapshots()

    def test_connection_refused_is_transport_error(self):
        dead = _forkd._ForkdController(f"http://127.0.0.1:{_closed_port()}", None, 2.0)
        with self.assertRaises(TransportError):
            dead.list_snapshots()

    def test_invalid_base_url_is_validation_error(self):
        with self.assertRaises(ValidationError):
            _forkd._ForkdController("not-a-url", None, 5.0)

    def test_controller_reuses_one_tcp_connection(self):
        # Regression: the controller client must pool one keep-alive HTTP
        # connection per RfbClient (10 requests -> 1 accept, like the Rust
        # reqwest pool); guest NDJSON/ZBRT stay connection-per-request.
        for _ in range(10):
            self.controller.ping_sandbox("sb-1")
        self.assertEqual(self.server.connections, 1)
        self.assertEqual(len(self.server.requests), 10)


if __name__ == "__main__":
    unittest.main()
