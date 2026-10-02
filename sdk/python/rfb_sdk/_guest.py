"""Internal forkd guest NDJSON-over-TCP client.

Mirrors ``rfb/src/forkd/guest.rs`` (wire behavior) with the facade-level
mapping of responses. This module is NOT part of the public API.
"""

import json
import math
import os
import socket
import time

from .errors import DecodeError, RemoteError, TransportError
from ._zbrt import _parse_host_port
from .models import StreamEvent, StreamEventKind

MAX_LINE_BYTES = 1024 * 1024

# When this environment variable is set to a non-empty value, every guest
# agent TCP connection starts with one auth line before any real request.
AGENT_TOKEN_ENV = "FORKD_AGENT_TOKEN"


def _agent_token():
    """Configured agent token, or None when auth is disabled (empty = unset)."""
    token = os.environ.get(AGENT_TOKEN_ENV)
    return token if token else None

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
        # NDJSON 温连接池：agent 的 serve 是循环的（一条连接可顺序承载多个
        # 请求）；每操作新建 TCP 连接的握手/拆除 ≈ 0.4ms/次。连接私有于
        # 一次操作期间；成功归还、失败即弃（任何失败都不重试——exec 的
        # 双重执行不可接受，rust guest.rs 参考语义）。
        self._pool = []

    def _effective_timeout(self, timeout_s):
        # Mirrors the Rust baseline (forkd/guest.rs): client timeout + exec
        # deadline + a fixed 5 s margin, so the guest's own timeout error is
        # what surfaces (not the client's).
        if timeout_s is None:
            return self._timeout_s
        return self._timeout_s + float(timeout_s) + 5.0

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

    def _authenticate(self, sock, rfile) -> None:
        """Send the agent auth line when FORKD_AGENT_TOKEN is configured.

        The auth line is the FIRST line on the connection, before any real
        request; the agent must answer ``{"action":"auth","ok":true}``. A
        rejection (or any non-auth reply) raises RemoteError so callers see
        the RfbError hierarchy (UNIFIED_API.md §7). With no token configured
        the wire behavior is unchanged.
        """
        token = _agent_token()
        if token is None:
            return
        line = json.dumps(
            {"action": "auth", "token": token}, separators=(",", ":")
        ).encode("utf-8") + b"\n"
        try:
            sock.sendall(line)
        except OSError as e:
            raise TransportError(f"guest write failed: {e}") from e
        while True:
            raw = self._read_line(rfile)
            if raw is None:
                continue  # blank keepalive line
            break
        value = self._decode_line(raw)
        if value.get("action") == "auth" and value.get("ok") is True:
            return
        error = value.get("error")
        detail = f": {error}" if isinstance(error, str) and error else ""
        raise RemoteError(f"guest agent auth failed{detail}")

    def _request(self, action: dict, timeout_s=None) -> list:
        timeout = self._effective_timeout(timeout_s)
        while self._pool:
            sock, rfile, ts = self._pool.pop()
            if time.monotonic() - ts >= 1.0:
                # NDJSON 无握手可验活：超龄连接直接丢弃重拨（同 rust
                # guest.rs 的池语义——热路径背靠背复用，空闲超龄不猜生死）。
                try:
                    sock.close()
                except OSError:
                    pass
                continue
            try:
                sock.settimeout(timeout)
                result = self._exchange(sock, rfile, action)
            except BaseException:
                try:
                    sock.close()
                except OSError:
                    pass
                raise
            # 成功路径必须归还，否则池每借一条就少一条（实测退化成隔次重拨）。
            self._repay(sock, rfile)
            return result
        try:
            sock = socket.create_connection(self._address, timeout=timeout)
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        except OSError as e:
            raise TransportError(f"guest connect failed: {e}") from e
        try:
            sock.settimeout(timeout)
            rfile = sock.makefile("rb")
            self._authenticate(sock, rfile)
            result = self._exchange(sock, rfile, action)
        except BaseException:
            sock.close()
            raise
        self._repay(sock, rfile)
        return result

    def _exchange(self, sock, rfile, action) -> list:
        try:
            line = json.dumps(action, separators=(",", ":")).encode("utf-8") + b"\n"
            try:
                sock.sendall(line)
            except OSError as e:
                raise TransportError(f"guest write failed: {e}") from e
            responses = []
            while True:
                raw = self._read_line(rfile)
                if raw is None:
                    continue
                value = self._decode_line(raw)
                responses.append(value)
                if any(key in value for key in TERMINAL_KEYS):
                    return responses
        except BaseException:
            try:
                rfile.close()
            except OSError:
                pass
            raise

    def _repay(self, sock, rfile) -> None:
        if len(self._pool) < 8:
            self._pool.append((sock, rfile, time.monotonic()))
        else:
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
            rfile = sock.makefile("rb")
            self._authenticate(sock, rfile)
            line = json.dumps(action, separators=(",", ":")).encode("utf-8") + b"\n"
            sock.sendall(line)
        except OSError as e:
            sock.close()
            raise TransportError(f"guest write failed: {e}") from e
        except BaseException:
            # Auth failures (RemoteError for a rejected token) escaped the
            # OSError handler and leaked both the socket and its reader.
            sock.close()
            rfile.close()
            raise
        # Reuse the already-buffered reader so bytes read ahead of the auth
        # exchange cannot be lost between two makefile() handles.
        return _NdjsonStream(sock, rfile)


class _NdjsonStream:
    """Interactive stream over one NDJSON TCP connection."""

    def __init__(self, sock: socket.socket, rfile=None):
        self._sock = sock
        self._rfile = rfile if rfile is not None else sock.makefile("rb")
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
