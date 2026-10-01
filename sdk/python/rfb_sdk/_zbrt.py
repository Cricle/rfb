"""Internal ZBRT v1 (ZeroBoot) frame codec and guest client.

Mirrors ``rfb-runtime/src/zeroboot_protocol.rs`` (wire contract) and
``rfb-runtime/src/zeroboot_connection.rs`` (session semantics). This module is
NOT part of the public API.
"""

import json
import math
import secrets
import socket
import struct
import threading

from .errors import DecodeError, RemoteError, TransportError
from .models import StreamEvent, StreamEventKind

MAGIC = b"ZBRT"
VERSION = 1
HEADER_LEN = 28
MAX_PAYLOAD = 16 * 1024 * 1024

KIND_HELLO = 1
KIND_HELLO_ACK = 2
KIND_EXECUTE = 3
KIND_OUTPUT = 4
KIND_EXIT = 5
KIND_CANCEL = 6
KIND_CANCEL_ACK = 7
KIND_FS = 8
KIND_FS_RESULT = 9
KIND_HEALTH = 10
KIND_HEALTH_ACK = 11
KIND_ERROR = 12
KIND_RESULT = 13

ZBRT_V1_CAPABILITIES = ["execute", "stream", "deadline", "health", "cancel", "filesystem"]

# Client name advertised in the mandatory Hello frame. A guest ZBRT connection
# must be helloed before anything else: the guest rejects every frame from a
# connection that has not completed Hello/HelloAck.
HELLO_CLIENT_NAME = "rfb-sdk-python"

# Aggregate cap on one exec turn's captured output (mirrors the Rust baseline
# ``rfb/src/client/zbrt.rs`` MAX_EXEC_BYTES so a chatty guest cannot grow host
# memory without bound). Exceeding it is a Remote-class error.
MAX_EXEC_BYTES = 16 * 1024 * 1024

STREAM_STDOUT = 0
STREAM_STDERR = 1

FS_OP_LS = 1
FS_OP_FIND = 2
FS_OP_GREP = 3
FS_OP_READ = 4
FS_OP_WRITE = 5


def new_request_id() -> bytes:
    """Fresh 128-bit request id per request."""
    return secrets.token_bytes(16)


# ---------------------------------------------------------------------------
# Frame layer
# ---------------------------------------------------------------------------


_HEADER_STRUCT = struct.Struct(">4sBBH16sI")


def encode_frame(kind: int, request_id: bytes, payload: bytes = b"") -> bytes:
    if not isinstance(request_id, (bytes, bytearray)) or len(request_id) != 16:
        raise ValueError("request_id must be exactly 16 bytes")
    if len(payload) > MAX_PAYLOAD:
        raise ValueError("payload too large")
    return _HEADER_STRUCT.pack(MAGIC, VERSION, kind, 0, request_id, len(payload)) + bytes(payload)


def _decode_header(data) -> tuple:
    """Validate a 28-byte frame prefix -> (kind, request_id, payload_len)."""
    if data[:4] != MAGIC:
        raise DecodeError("invalid frame magic")
    if data[4] != VERSION:
        raise DecodeError("unsupported frame version")
    kind = data[5]
    if not 1 <= kind <= KIND_RESULT:
        raise DecodeError(f"unknown frame kind {kind}")
    (flags,) = struct.unpack_from(">H", data, 6)
    if flags != 0:
        raise DecodeError("unsupported frame flags")
    (payload_len,) = struct.unpack_from(">I", data, 24)
    if payload_len > MAX_PAYLOAD:
        raise DecodeError("payload too large")
    return kind, data[8:24], payload_len


def decode_frame(data) -> tuple:
    """Strictly decode exactly one frame from a byte buffer."""
    data = bytes(data)
    if len(data) < HEADER_LEN:
        raise DecodeError("truncated frame header")
    kind, request_id, payload_len = _decode_header(data)
    if len(data) < HEADER_LEN + payload_len:
        raise DecodeError("truncated frame payload")
    if len(data) > HEADER_LEN + payload_len:
        raise DecodeError("trailing bytes after frame")
    return kind, request_id, data[HEADER_LEN : HEADER_LEN + payload_len]


def _recv_exact(sock, n: int):
    buf = bytearray()
    while len(buf) < n:
        try:
            chunk = sock.recv(n - len(buf))
        except socket.timeout as e:
            raise TransportError("zbrt read timeout") from e
        except OSError as e:
            raise TransportError(f"zbrt read failed: {e}") from e
        if not chunk:
            return None
        buf += chunk
    return bytes(buf)


def read_frame(sock):
    """Read one frame from a socket; ``None`` on clean EOF at a frame boundary."""
    header = _recv_exact(sock, HEADER_LEN)
    if header is None:
        return None
    if len(header) < HEADER_LEN:
        raise TransportError("zbrt connection closed mid-frame")
    kind, request_id, payload_len = _decode_header(header)
    payload = _recv_exact(sock, payload_len)
    if payload is None:
        raise TransportError("zbrt connection closed mid-frame")
    return kind, request_id, payload


def write_frame(sock, kind: int, request_id: bytes, payload: bytes = b"") -> None:
    data = encode_frame(kind, request_id, payload)
    try:
        sock.sendall(data)
    except socket.timeout as e:
        raise TransportError("zbrt write timeout") from e
    except OSError as e:
        raise TransportError(f"zbrt write failed: {e}") from e


# ---------------------------------------------------------------------------
# Strict payload codecs (u32 BE length-prefixed fields; reject truncation and
# trailing bytes)
# ---------------------------------------------------------------------------


class _Cursor:
    """Strict payload reader: no intermediate slices for fixed-width fields."""

    __slots__ = ("_data", "_len", "_pos")

    def __init__(self, data):
        self._data = bytes(data)
        self._len = len(self._data)
        self._pos = 0

    def _take(self, n: int) -> bytes:
        end = self._pos + n
        if end > self._len:
            raise DecodeError("truncated payload")
        out = self._data[self._pos:end]
        self._pos = end
        return out

    def u8(self) -> int:
        pos = self._pos
        if pos >= self._len:
            raise DecodeError("truncated payload")
        self._pos = pos + 1
        return self._data[pos]

    def flag(self) -> bool:
        return self.u8() != 0

    def u32(self) -> int:
        pos = self._pos
        if pos + 4 > self._len:
            raise DecodeError("truncated payload")
        self._pos = pos + 4
        return struct.unpack_from(">I", self._data, pos)[0]

    def i32(self) -> int:
        pos = self._pos
        if pos + 4 > self._len:
            raise DecodeError("truncated payload")
        self._pos = pos + 4
        return struct.unpack_from(">i", self._data, pos)[0]

    def bytes_field(self) -> bytes:
        return self._take(self.u32())

    def str_field(self) -> str:
        raw = self.bytes_field()
        try:
            return raw.decode("utf-8")
        except UnicodeDecodeError as e:
            raise DecodeError("invalid utf-8 in payload") from e

    def finish(self) -> None:
        if self._pos != self._len:
            raise DecodeError("trailing payload bytes")


def _put_str(out: bytearray, value: str) -> None:
    raw = value.encode("utf-8")
    out += struct.pack(">I", len(raw))
    out += raw


def _put_bytes(out: bytearray, value) -> None:
    out += struct.pack(">I", len(value))
    out += value


def encode_hello(client: str, capabilities) -> bytes:
    out = bytearray()
    _put_str(out, client)
    if len(capabilities) > 255:
        raise ValueError("too many capabilities")
    out.append(len(capabilities))
    for cap in capabilities:
        _put_str(out, cap)
    return bytes(out)


def decode_hello(payload) -> tuple:
    cur = _Cursor(payload)
    client = cur.str_field()
    count = cur.u8()
    capabilities = [cur.str_field() for _ in range(count)]
    cur.finish()
    return client, capabilities


def encode_hello_ack(server: str, capabilities) -> bytes:
    return encode_hello(server, capabilities)


def decode_hello_ack(payload) -> tuple:
    return decode_hello(payload)


def encode_execute(argv, cwd, stdin, timeout_ms: int) -> bytes:
    out = bytearray()
    if len(argv) > 255:
        raise ValueError("too many arguments")
    out.append(len(argv))
    for arg in argv:
        _put_str(out, arg)
    if cwd is None:
        out.append(0)
    else:
        out.append(1)
        _put_str(out, cwd)
    _put_bytes(out, stdin)
    out += struct.pack(">I", timeout_ms)
    return bytes(out)


def decode_execute(payload) -> tuple:
    cur = _Cursor(payload)
    argc = cur.u8()
    argv = [cur.str_field() for _ in range(argc)]
    cwd = cur.str_field() if cur.flag() else None
    stdin = cur.bytes_field()
    timeout_ms = cur.u32()
    cur.finish()
    return argv, cwd, stdin, timeout_ms


def encode_output(stream: int, data) -> bytes:
    out = bytearray([stream])
    _put_bytes(out, data)
    return bytes(out)


def decode_output(payload) -> tuple:
    cur = _Cursor(payload)
    stream = cur.u8()
    data = cur.bytes_field()
    cur.finish()
    return stream, data


def encode_exit(code: int, signal) -> bytes:
    out = bytearray(struct.pack(">i", code))
    if signal is None:
        out.append(0)
    else:
        out.append(1)
        out += struct.pack(">I", signal)
    return bytes(out)


def decode_exit(payload) -> tuple:
    cur = _Cursor(payload)
    code = cur.i32()
    signal = cur.u32() if cur.flag() else None
    cur.finish()
    return code, signal


def encode_cancel(reason, target=None, include_target_field: bool = True) -> bytes:
    out = bytearray()
    if reason is None:
        out.append(0)
    else:
        out.append(1)
        _put_str(out, reason)
    if not include_target_field:
        if target is not None:
            raise ValueError("legacy cancel cannot carry a target")
        return bytes(out)
    if target is None:
        out.append(0)
    else:
        out.append(1)
        out += bytes(target)
    return bytes(out)


def decode_cancel(payload) -> tuple:
    """Decode Cancel; legacy payloads (payload ends after the reason flag)
    decode as target=None."""
    cur = _Cursor(payload)
    reason = cur.str_field() if cur.flag() else None
    if cur._pos == cur._len:
        return reason, None  # legacy V1 payload: no target byte
    target = cur._take(16) if cur.flag() else None
    cur.finish()
    return reason, target


def encode_fs(op: int, path: str, data) -> bytes:
    out = bytearray([op])
    _put_str(out, path)
    _put_bytes(out, data)
    return bytes(out)


def decode_fs(payload) -> tuple:
    cur = _Cursor(payload)
    op = cur.u8()
    path = cur.str_field()
    data = cur.bytes_field()
    cur.finish()
    return op, path, data


def encode_health(healthy: bool, message) -> bytes:
    out = bytearray([1 if healthy else 0])
    if message is None:
        out.append(0)
    else:
        out.append(1)
        _put_str(out, message)
    return bytes(out)


def decode_health(payload) -> tuple:
    cur = _Cursor(payload)
    healthy = cur.flag()
    message = cur.str_field() if cur.flag() else None
    cur.finish()
    return healthy, message


def encode_error(code: int, message: str) -> bytes:
    out = bytearray(struct.pack(">I", code))
    _put_str(out, message)
    return bytes(out)


def decode_error(payload) -> tuple:
    cur = _Cursor(payload)
    code = cur.u32()
    message = cur.str_field()
    cur.finish()
    return code, message


# ---------------------------------------------------------------------------
# Client
# ---------------------------------------------------------------------------


def _parse_host_port(address: str) -> tuple:
    host, sep, port_text = address.rpartition(":")
    if not sep or not host:
        raise DecodeError(f"invalid guest address: {address!r}")
    try:
        port = int(port_text)
    except ValueError as e:
        raise DecodeError(f"invalid guest address: {address!r}") from e
    return host, port


class _ZbrtGuestClient:
    """One-shot ZBRT v1 request client (fresh connection per request)."""

    def __init__(self, address: str, timeout_s: float = 10.0):
        self._address = _parse_host_port(address)
        self._timeout_s = timeout_s
        # health/fs RPC 复用一条控制连接（rust zbrt.rs 的 §3.4 语义）；
        # exec/stream 保持每 turn 一条新连接（UNIFIED_API §3.4）。
        self._control = None
        self._control_lock = threading.Lock()
        # exec 温连接池：新连接握手是毫秒级成本；一条连接可顺序跑多个 turn
        # （Hello 幂等），池保住已握手的空闲连接，exec 借还即用。
        self._exec_pool = []

    def drop_control(self) -> None:
        """Close the cached control connection (sandbox deleted / shutdown)."""
        with self._control_lock:
            self._drop_unlocked()

    def _drop_unlocked(self) -> None:
        if self._control is not None:
            try:
                self._control.close()
            except OSError:
                pass
            self._control = None

    def _pop_exec_pool(self, timeout_s: float):
        """Borrow a warm exec connection (re-Hello = liveness probe), or None."""
        while self._exec_pool:
            sock = self._exec_pool.pop()
            try:
                sock.settimeout(timeout_s)
                self._handshake(sock)
                return sock
            except (TransportError, DecodeError, OSError):
                try:
                    sock.close()
                except OSError:
                    pass
        return None

    def _repay_exec_pool(self, sock: socket.socket) -> None:
        if len(self._exec_pool) < 4:
            self._exec_pool.append(sock)
        else:
            try:
                sock.close()
            except OSError:
                pass

    @staticmethod
    def _retryable(error: TransportError) -> bool:
        """rust zbrt.rs 的重试分类：读超时 = 请求可能已在 guest 执行，绝不
        重试；写失败 / 连接断开（EOF、OSError）换新连接重试一次。"""
        if str(error) == "zbrt read timeout":
            return False
        if isinstance(error, DecodeError):
            return str(error).startswith("truncated frame")
        return True

    def _control_exchange(self, send, parse):
        """One health/fs exchange over the cached control connection."""
        with self._control_lock:
            for reuse in (True, False):
                sock = self._control if reuse else None
                if sock is None:
                    self._drop_unlocked()
                    sock = self._connect(self._timeout_s)
                try:
                    request_id = send(sock)
                    reply = self._read_reply(sock, request_id)
                except TransportError as e:
                    self._drop_unlocked()
                    if reuse and self._retryable(e):
                        continue
                    raise
                self._control = sock
                return parse(reply)
            raise TransportError("zbrt control exchange exhausted")

    def _connect(self, timeout_s: float) -> socket.socket:
        try:
            sock = socket.create_connection(self._address, timeout=timeout_s)
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        except OSError as e:
            raise TransportError(f"zbrt connect failed: {e}") from e
        self._handshake(sock)
        return sock

    @staticmethod
    def _handshake(sock: socket.socket) -> None:
        """Mandatory Hello/HelloAck handshake on a freshly connected socket.

        The guest refuses every frame until the connection is helloed, so each
        connection opens with Hello (client name + full V1 capability set) and
        must be answered by an echoing HelloAck. Every handshake failure —
        closed peer, mismatched id, wrong kind, malformed payload — surfaces as
        a TransportError (cross-language contract: handshake failures are
        transport failures).
        """
        request_id = new_request_id()
        try:
            write_frame(
                sock,
                KIND_HELLO,
                request_id,
                encode_hello(HELLO_CLIENT_NAME, ZBRT_V1_CAPABILITIES),
            )
            frame = read_frame(sock)
        except DecodeError as e:
            raise TransportError(f"zbrt hello handshake failed: {e}") from e
        if frame is None:
            raise TransportError("zbrt connection closed during hello handshake")
        kind, reply_id, payload = frame
        if reply_id != request_id:
            raise TransportError("zbrt hello handshake request id mismatch")
        if kind == KIND_ERROR:
            try:
                message = decode_error(payload)[1] if payload else "rejected"
            except DecodeError as e:
                # A codec fault inside a Hello rejection is still a handshake
                # failure: every handshake outcome maps to the Transport class
                # (a malformed Error payload must not leak as DecodeError).
                raise TransportError(
                    f"zbrt hello handshake failed: invalid error payload: {e}"
                ) from e
            raise TransportError(f"zbrt hello handshake rejected: {message}")
        if kind != KIND_HELLO_ACK:
            raise TransportError(f"unexpected zbrt frame kind {kind} during hello handshake")
        try:
            decode_hello_ack(payload)
        except DecodeError as e:
            raise TransportError(f"invalid zbrt hello ack payload: {e}") from e

    def _effective_timeout(self, timeout_s) -> float:
        if not timeout_s:
            return self._timeout_s
        return max(self._timeout_s, float(timeout_s) + 5.0)

    def _read_reply(self, sock, request_id: bytes):
        frame = read_frame(sock)
        if frame is None:
            raise TransportError("zbrt connection closed before reply")
        kind, reply_id, payload = frame
        if reply_id != request_id:
            # PROTOCOL.md §3.4: a reply whose request_id does not match the
            # request is a decode failure (a stale/mismatched frame must never
            # be accepted as this request's answer).
            raise DecodeError("zbrt reply request id mismatch")
        return kind, reply_id, payload

    def _exec(self, argv, cwd, timeout_s, stdin) -> tuple:
        read_timeout = self._effective_timeout(timeout_s)
        # 温连接借出（借出前已重发 Hello 验活——连接已死则回退新连接）；
        # 写失败 = 请求未送达，换新连接重试一次；读侧失败绝不重试。
        sock = self._pop_exec_pool(read_timeout)
        if sock is not None:
            try:
                return self._exec_turn(sock, argv, cwd, timeout_s, stdin,
                                       read_timeout, pooled=True)
            except TransportError as e:
                try:
                    sock.close()
                except OSError:
                    pass
                if str(e).startswith("zbrt write"):
                    sock = self._connect(read_timeout)
                    return self._exec_turn(sock, argv, cwd, timeout_s, stdin,
                                           read_timeout, pooled=False)
                raise
        sock = self._connect(read_timeout)
        return self._exec_turn(sock, argv, cwd, timeout_s, stdin,
                               read_timeout, pooled=False)

    def _exec_turn(self, sock, argv, cwd, timeout_s, stdin, read_timeout,
                   pooled: bool) -> tuple:
        repaid = False
        try:
            request_id = new_request_id()
            # Rust baseline (facade.rs GuestOps::exec / ::eval ZBRT branch):
            # whole seconds (ceil, min 1) times 1000, capped at u32::MAX;
            # timeout_s unset -> 0.
            if timeout_s:
                secs = max(1, math.ceil(float(timeout_s)))
                timeout_ms = min(secs * 1000, 0xFFFFFFFF)
            else:
                timeout_ms = 0
            write_frame(sock, KIND_EXECUTE, request_id, encode_execute(argv, cwd, stdin, timeout_ms))
            stdout = bytearray()
            stderr = bytearray()
            total = 0
            while True:
                kind, _rid, payload = self._read_reply(sock, request_id)
                if kind == KIND_OUTPUT:
                    stream, data = decode_output(payload)
                    # Aggregate cap across the whole turn (Rust baseline
                    # client/zbrt.rs): exceed -> Remote-class error.
                    total += len(data)
                    if total > MAX_EXEC_BYTES:
                        raise RemoteError("guest output exceeded the 16 MiB limit")
                    if stream == STREAM_STDOUT:
                        stdout += data
                    elif stream == STREAM_STDERR:
                        stderr += data
                    else:
                        raise DecodeError(f"invalid zbrt output stream id {stream}")
                elif kind == KIND_EXIT:
                    code, _signal = decode_exit(payload)
                    # 干净结束一律归还（新连接也回池）——否则池永远是空的。
                    self._repay_exec_pool(sock)
                    repaid = True
                    return int(code), bytes(stdout), bytes(stderr)
                elif kind == KIND_ERROR:
                    raise RemoteError(decode_error(payload)[1])
                else:
                    raise DecodeError(f"unexpected zbrt frame kind {kind} during execute")
        finally:
            if not repaid:
                sock.close()

    def exec(self, args, cwd, timeout_s, stdin=b"") -> tuple:
        return self._exec(list(args), cwd, timeout_s, bytes(stdin))

    def ping(self) -> bool:
        def send(sock):
            request_id = new_request_id()
            write_frame(sock, KIND_HEALTH, request_id, encode_health(True, None))
            return request_id

        def parse(reply):
            kind, _rid, payload = reply
            if kind == KIND_ERROR:
                raise RemoteError(decode_error(payload)[1])
            if kind != KIND_HEALTH_ACK:
                raise DecodeError(f"unexpected zbrt frame kind {kind} for health")
            healthy, _message = decode_health(payload)
            return healthy

        return self._control_exchange(send, parse)

    def fs_op(self, op: int, path: str, args: dict) -> dict:
        def send(sock):
            data = json.dumps(args, separators=(",", ":")).encode("utf-8")
            request_id = new_request_id()
            write_frame(sock, KIND_FS, request_id, encode_fs(op, path, data))
            return request_id

        def parse(reply):
            kind, _rid, payload = reply
            if kind == KIND_ERROR:
                raise RemoteError(decode_error(payload)[1])
            if kind != KIND_FS_RESULT:
                raise DecodeError(f"unexpected zbrt frame kind {kind} for fs op {op}")
            try:
                value = json.loads(payload.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError) as e:
                raise DecodeError(f"invalid zbrt fs result json: {e}") from e
            if not isinstance(value, dict):
                raise DecodeError("zbrt fs result must be a JSON object")
            return value

        return self._control_exchange(send, parse)

    def open_stream(self, args, cwd) -> "_ZbrtStream":
        sock = self._connect(self._timeout_s)
        try:
            request_id = new_request_id()
            write_frame(sock, KIND_EXECUTE, request_id, encode_execute(list(args), cwd, b"", 0))
        except BaseException:
            sock.close()
            raise
        return _ZbrtStream(sock, request_id)


class _ZbrtStream:
    """Interactive stream over one ZBRT connection (single active request)."""

    def __init__(self, sock: socket.socket, request_id: bytes):
        self._sock = sock
        self._request_id = request_id
        self._stopped = False
        self._terminal = False
        self._closed = False
        self._pending = []
        # ZBRT has no started frame: the first next_event synthesizes one
        # (Rust baseline client/zbrt.rs ZbrtStream::next_event).
        self._started_sent = False

    def next_event(self):
        if not self._started_sent:
            self._started_sent = True
            return StreamEvent(StreamEventKind.STARTED)
        if self._pending:
            return self._pending.pop(0)
        if self._terminal or self._closed:
            return None
        frame = read_frame(self._sock)
        if frame is None:
            self._terminal = True
            self._close()
            return None
        kind, reply_id, payload = frame
        if reply_id != self._request_id:
            raise DecodeError("zbrt reply request id mismatch")
        if kind == KIND_OUTPUT:
            stream, data = decode_output(payload)
            if stream == STREAM_STDOUT:
                return StreamEvent(StreamEventKind.STDOUT, bytes(data))
            if stream == STREAM_STDERR:
                return StreamEvent(StreamEventKind.STDERR, bytes(data))
            raise DecodeError(f"invalid zbrt output stream id {stream}")
        if kind == KIND_EXIT:
            code, _signal = decode_exit(payload)
            self._terminal = True
            self._close()
            return StreamEvent(StreamEventKind.EXIT, code=int(code))
        if kind == KIND_ERROR:
            self._terminal = True
            self._close()
            raise RemoteError(decode_error(payload)[1])
        raise DecodeError(f"unexpected zbrt frame kind {kind} in stream")

    def send_input(self, text: str) -> None:
        if self._terminal or self._stopped or self._closed:
            raise RemoteError("guest stream is no longer running")
        # ZBRT v1 carries stdin only inside the Execute payload; there is no
        # wire message to push stdin to an already-submitted request.
        raise RemoteError("stdin is not supported over the zbrt transport")

    def stop(self) -> None:
        """Idempotent: Cancel is acknowledged with an empty-payload CancelAck."""
        if self._terminal or self._stopped or self._closed:
            return
        self._stopped = True
        # The Cancel frame header reuses the id of the request being cancelled
        # so the CancelAck routes back to this turn and passes the id check
        # (Rust baseline: client/zbrt.rs ZbrtStream::stop).
        write_frame(
            self._sock,
            KIND_CANCEL,
            self._request_id,
            encode_cancel("stop", self._request_id),
        )
        while True:
            frame = read_frame(self._sock)
            if frame is None:
                self._terminal = True
                self._close()
                return
            kind, reply_id, payload = frame
            if reply_id != self._request_id:
                raise DecodeError("zbrt reply request id mismatch")
            if kind == KIND_CANCEL_ACK:
                return
            if kind == KIND_OUTPUT:
                # Buffer straggler output that arrives before the ack.
                stream, data = decode_output(payload)
                if stream == STREAM_STDOUT:
                    self._pending.append(StreamEvent(StreamEventKind.STDOUT, bytes(data)))
                elif stream == STREAM_STDERR:
                    self._pending.append(StreamEvent(StreamEventKind.STDERR, bytes(data)))
                else:
                    raise DecodeError(f"invalid zbrt output stream id {stream}")
                continue
            if kind == KIND_EXIT:
                # The turn already terminated: cancel is trivially complete.
                code, _signal = decode_exit(payload)
                self._terminal = True
                self._pending.append(StreamEvent(StreamEventKind.EXIT, code=int(code)))
                self._close()
                return
            if kind == KIND_ERROR:
                self._terminal = True
                self._close()
                raise RemoteError(decode_error(payload)[1])
            raise DecodeError(f"unexpected zbrt frame kind {kind} for cancel")

    def _close(self) -> None:
        if not self._closed:
            self._closed = True
            try:
                self._sock.close()
            except OSError:
                pass
