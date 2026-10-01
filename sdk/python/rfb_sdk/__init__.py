"""RFB unified SDK (Python).

Python mirror of the Rust reference implementation ``rfb::client``. The three
protocol adapters (forkd controller HTTP, forkd guest NDJSON, ZBRT v1 frames)
live in underscore-prefixed internal modules and are not exported here.
"""

from .errors import (
    DecodeError,
    HttpStatusError,
    RemoteError,
    RfbError,
    TransportError,
    ValidationError,
)
from .facade import (
    DEFAULT_ZBRT_TCP,
    TRANSPORT_NDJSON,
    TRANSPORT_ZBRT,
    GuestStream,
    RfbClient,
    Sandbox,
)
from .host import ForkdHost, TcpVsockRelay, ZerobootHost
from .models import (
    DirEntry,
    ExecResult,
    FileRead,
    GrepMatch,
    SandboxInfo,
    Snapshot,
    StreamEvent,
    StreamEventKind,
)

__all__ = [
    "DEFAULT_ZBRT_TCP",
    "TRANSPORT_NDJSON",
    "TRANSPORT_ZBRT",
    "RfbClient",
    "ZerobootHost",
    "ForkdHost",
    "TcpVsockRelay",
    "Sandbox",
    "GuestStream",
    "StreamEvent",
    "StreamEventKind",
    "ExecResult",
    "DirEntry",
    "GrepMatch",
    "FileRead",
    "Snapshot",
    "SandboxInfo",
    "RfbError",
    "TransportError",
    "HttpStatusError",
    "DecodeError",
    "RemoteError",
    "ValidationError",
]
