"""eval over ZBRT fails closed (no eval opcode on the ZBRT v1 wire).

ZBRT v1 has no eval opcode and the reference guest maps Execute verbatim onto
``exec``: the old "facade convention" (argv=["eval", code]) surfaced the
guest's ``eval: not found`` exit code as a successful result. The facade now
rejects eval over ZBRT locally, with zero frames on the wire — mirroring the
Rust baseline (``rfb::client::GuestOps::eval``).
"""

import unittest

from rfb_sdk import RfbClient
from rfb_sdk.errors import ValidationError

from tests.fake_servers import FakeControllerServer, FakeZbrtServer


class EvalZbrtFacadeTests(unittest.TestCase):
    """Facade-level eval-over-ZBRT fail-closed regression."""

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

    def test_eval_fails_closed_without_sending_frames(self):
        guest = FakeZbrtServer()
        guest.start()
        self.addCleanup(guest.stop)
        sandbox = self._zbrt_sandbox(guest)

        with self.assertRaises(ValidationError):
            sandbox.eval("print(40+2)")
        with self.assertRaises(ValidationError):
            sandbox.eval("1+1", cwd="/workspace", timeout_s=5.0)

        # Local validation still runs first and also stays off the wire.
        with self.assertRaises(ValidationError):
            sandbox.eval("   ")

        self.assertEqual(len(guest.received_executes), 0, "no frames sent")
        self.assertEqual(guest.connections, 0, "no connections opened")


if __name__ == "__main__":
    unittest.main()
