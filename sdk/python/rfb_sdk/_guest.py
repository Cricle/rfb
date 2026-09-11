"""Internal forkd guest NDJSON-over-TCP client.

Mirrors ``rfb/src/forkd/guest.rs`` (wire behavior) with the facade-level
mapping of responses. This module is NOT part of the public API.
"""

import json
import math
import socket

from .errors import DecodeError, RemoteError, TransportError
from ._zbrt import _parse_host_port
from .models import StreamEvent, StreamEventKind

MAX_LINE_BYTES = 1024 * 1024

# A response line containing any of these keys is terminal (PROTOCOL.md 2.1).
TERMINAL_KEYS = frozenset(
    {
        "exit_code",
        "pong",
        "results",
        "entries",
        "matches",
        "data",
        "content",
        "output",
        "status",
        "ok",
        "healthy",
        "done",
        "cancelled",
        "bytes_written",
    }
)


def _as_bytes(value) -> bytes:
    if value is None:
        return b""
    if isinstance(value, (bytes, bytearray)):
        return bytes(value)
    if isinstance(value, str):
        return value.encode("utf-8")
    if isinstance(value, list):
        return bytes(bytearray(value))
    raise DecodeError("expected a byte array or string")


def _first_of(value: dict, current: str, legacy: str):
    """Prefer the current response key; fall back to the legacy agent key.

    Older forkd guests emit ``out`` / ``err`` where current guests emit
    ``stdout`` / ``stderr`` (and legacy eval output under ``out``); both are
    accepted so old guests keep working.
    """
    if value.get(current) is not None:
        return value[current]
    return value.get(legacy)


class _GuestNdjsonClient:
    """One-shot NDJSON request client (fresh TCP connection per request)."""

    def __init__(self, address: str, timeout_s: float = 10.0):
        self._address = _parse_host_port(address)
        self._timeout_s = timeout_s

    def _effective_timeout(self, timeout_s):
        if timeout_s is None:
            return self._timeout_s
        return max(self._timeout_s, float(timeout_s) + 5.0)

    def _read_line(self, rfile) -> bytes:
        try:
            raw = rfile.readline(MAX_LINE_BYTES + 1)
        except (TimeoutError, socket.timeout) as e:
            # Every timeout must surface as a TransportError so callers can
            # catch the RfbError hierarchy (UNIFIED_API.md §7).
            raise TransportError("guest read timeout") from e
        except OSError as e:
            raise TransportError(f"guest read failed: {e}") from e
        if not raw:
            raise RemoteError("guest closed before response")
        if len(raw) > MAX_LINE_BYTES or not raw.endswith(b"\n"):
            raise DecodeError("guest response line exceeds 1 MiB or is unterminated")
        raw = raw.rstrip(b"\r\n")
        if not raw:
            return None
        return raw

    @staticmethod
    def _decode_line(raw: bytes) -> dict:
        try:
            value = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as e:
            raise DecodeError(f"invalid guest json: {e}") from e
        if not isinstance(value, dict):
            raise DecodeError("guest response line must be a JSON object")
        error = value.get("error")
        if isinstance(error, str):
            raise RemoteError(error)
        return value

    def _request(self, action: dict, timeout_s=None) -> list:
        timeout = self._effective_timeout(timeout_s)
        try:
            sock = socket.create_connection(self._address, timeout=timeout)
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        except OSError as e:
            raise TransportError(f"guest connect failed: {e}") from e
        try:
            sock.settimeout(timeout)
            line = json.dumps(action, separators=(",", ":")).encode("utf-8") + b"\n"
            try:
                sock.sendall(line)
            except OSError as e:
                raise TransportError(f"guest write failed: {e}") from e
            rfile = sock.makefile("rb")
            responses = []
            while True:
                raw = self._read_line(rfile)
                if raw is None:
                    continue
                value = self._decode_line(raw)
                responses.append(value)
                if any(key in value for key in TERMINAL_KEYS):
                    return responses
        finally:
            sock.close()

    def ping(self) -> dict:
        return self._request({"action": "ping"})[-1]

    def exec(self, cwd: str, args, timeout_s) -> dict:
        action = {
            "action": "exec",
            "cwd": cwd,
            "args": list(args),
            # Rust baseline (validation::timeout_secs): whole seconds on the
            # wire — ceil with a minimum of 1, same as the eval path.
            "timeout": max(1, math.ceil(float(timeout_s))),
        }
        # `_request` applies the timeout slack exactly once.
        return self._request(action, timeout_s)[-1]

    def eval(self, code: str, cwd, timeout_s) -> dict:
        action = {"action": "eval", "code": code}
        if cwd is not None:
            action["cwd"] = cwd
        if timeout_s is not None:
            # RFB durations are milliseconds; forkd's eval timeout is seconds,
            # so round up to whole seconds with a minimum of 1.
            action["timeout"] = max(1, math.ceil(float(timeout_s)))
        return self._request(action, timeout_s)[-1]

    def tool(self, action: dict) -> dict:
        return self._request(action)[-1]

    def stream(self, args, cwd, pty, env) -> "_NdjsonStream":
        action = {"action": "stream", "args": list(args)}
        if cwd is not None:
            action["cwd"] = cwd
        if pty is not None:
            action["pty"] = bool(pty)
        if env is not None:
            action["env"] = env
        try:
            sock = socket.create_connection(self._address, timeout=self._timeout_s)
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        except OSError as e:
            raise TransportError(f"guest connect failed: {e}") from e
        try:
            sock.settimeout(self._timeout_s)
            line = json.dumps(action, separators=(",", ":")).encode("utf-8") + b"\n"
            sock.sendall(line)
        except OSError as e:
            sock.close()
            raise TransportError(f"guest write failed: {e}") from e
        return _NdjsonStream(sock)


class _NdjsonStream:
    """Interactive stream over one NDJSON TCP connection."""

    def __init__(self, sock: socket.socket):
        self._sock = sock
        self._rfile = sock.makefile("rb")
        self._stopped = False
        self._terminal = False
        self._closed = False

    def _write_line(self, value: dict) -> None:
        line = json.dumps(value, separators=(",", ":")).encode("utf-8") + b"\n"
        try:
            self._sock.sendall(line)
        except OSError as e:
            raise TransportError(f"guest write failed: {e}") from e

    def next_event(self):
        if self._terminal or self._closed:
            return None
        while True:
            try:
                raw = self._rfile.readline(MAX_LINE_BYTES + 1)
            except (TimeoutError, socket.timeout) as e:
                raise TransportError("guest stream read timeout") from e
            except OSError as e:
                raise TransportError(f"guest stream read failed: {e}") from e
            if not raw:
                self._terminal = True
                self._close()
                return None
            if len(raw) > MAX_LINE_BYTES or not raw.endswith(b"\n"):
                raise DecodeError("guest stream line exceeds 1 MiB or is unterminated")
            raw = raw.rstrip(b"\r\n")
            if not raw:
                continue
            value = _GuestNdjsonClient._decode_line(raw)
            if "exit_code" in value:
                self._terminal = True
                self._close()
                code = value["exit_code"]
                return StreamEvent(
                    StreamEventKind.EXIT,
                    code=int(code) if isinstance(code, int) else None,
                )
            if "stdout" in value or "out" in value:
                return StreamEvent(
                    StreamEventKind.STDOUT,
                    _as_bytes(_first_of(value, "stdout", "out")),
                )
            if "stderr" in value or "err" in value:
                return StreamEvent(
                    StreamEventKind.STDERR,
                    _as_bytes(_first_of(value, "stderr", "err")),
                )
            if value.get("event") == "started" or value.get("started") is True:
                return StreamEvent(StreamEventKind.STARTED)
            # Other non-terminal lines are ignored.

    def send_input(self, text: str) -> None:
        if self._terminal or self._stopped or self._closed:
            raise RemoteError("guest stream is no longer running")
        self._write_line({"in": text})

    def stop(self) -> None:
        if self._terminal or self._stopped or self._closed:
            return
        self._stopped = True
        self._write_line({"action": "stop"})

    def _close(self) -> None:
        if not self._closed:
            self._closed = True
            try:
                self._sock.close()
            except OSError:
                pass
