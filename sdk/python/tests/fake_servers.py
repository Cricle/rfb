"""In-process fake servers for the rfb SDK test suite.

- FakeControllerServer: minimal forkd controller (HTTP/JSON).
- FakeNdjsonGuestServer: minimal forkd guest (TCP + newline-delimited JSON).
- FakeZbrtServer: minimal ZBRT v1 frame server (independent hand-rolled
  frame reader/writer so the tests do not trust the SDK's own codec).
"""

import http.server
import json
import socketserver
import struct
import threading
import urllib.parse

ZBRT_MAGIC = b"ZBRT"

KIND_HELLO_ACK = 2
KIND_EXECUTE = 3
KIND_OUTPUT = 4
KIND_EXIT = 5
KIND_CANCEL_ACK = 7
KIND_FS_RESULT = 9
KIND_HEALTH_ACK = 11
KIND_ERROR = 12

# Independent frame I/O (not importing rfb_sdk._zbrt on purpose).


def _recv_exact(rfile, n):
    buf = b""
    while len(buf) < n:
        chunk = rfile.read(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return buf


def read_frame(rfile):
    header = _recv_exact(rfile, 28)
    if header is None:
        return None
    assert header[:4] == ZBRT_MAGIC, "bad magic sent by SDK"
    kind = header[5]
    request_id = header[8:24]
    (payload_len,) = struct.unpack(">I", header[24:28])
    payload = _recv_exact(rfile, payload_len)
    assert payload is not None, "truncated payload sent by SDK"
    return kind, request_id, payload


def write_frame(wfile, kind, request_id, payload=b""):
    wfile.write(ZBRT_MAGIC)
    wfile.write(struct.pack(">BBH", 1, kind, 0))
    wfile.write(request_id)
    wfile.write(struct.pack(">I", len(payload)))
    wfile.write(payload)
    wfile.flush()


def parse_execute(payload):
    """argv, cwd, stdin, timeout_ms from an Execute payload."""
    pos = 0
    argc = payload[pos]
    pos += 1
    argv = []
    for _ in range(argc):
        (n,) = struct.unpack_from(">I", payload, pos)
        pos += 4
        argv.append(payload[pos : pos + n].decode("utf-8"))
        pos += n
    has_cwd = payload[pos]
    pos += 1
    cwd = None
    if has_cwd:
        (n,) = struct.unpack_from(">I", payload, pos)
        pos += 4
        cwd = payload[pos : pos + n].decode("utf-8")
        pos += n
    (stdin_len,) = struct.unpack_from(">I", payload, pos)
    pos += 4
    stdin = payload[pos : pos + stdin_len]
    pos += stdin_len
    (timeout_ms,) = struct.unpack_from(">I", payload, pos)
    return argv, cwd, stdin, timeout_ms


class _ThreadedTcpServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


class _CountingHttpServer(http.server.ThreadingHTTPServer):
    """ThreadingHTTPServer that counts accepted TCP connections."""

    daemon_threads = True
    connections = 0

    def process_request(self, request, client_address):
        self.connections += 1
        super().process_request(request, client_address)


# ---------------------------------------------------------------------------
# forkd controller fake
# ---------------------------------------------------------------------------


class FakeControllerServer:
    """Minimal forkd controller with scriptable responses."""

    def __init__(
        self,
        *,
        snapshots=None,
        snapshot_by_tag=None,
        sandboxes=None,
        info_404_tags=(),
        delete_404_ids=(),
        guest_addr="127.0.0.1:9000",
        behavior=None,
    ):
        self.snapshots = snapshots or []
        self.snapshot_by_tag = snapshot_by_tag or {}
        self.sandboxes = list(sandboxes or [])
        self.info_404_tags = set(info_404_tags)
        self.delete_404_ids = set(delete_404_ids)
        self.guest_addr = guest_addr
        # behavior(method, path, body_bytes) -> (status, payload) | None
        self.behavior = behavior
        self.requests = []
        self._next_id = 0
        self._server = _CountingHttpServer(("127.0.0.1", 0), self._make_handler())
        self._thread = threading.Thread(target=self._server.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)

    @property
    def port(self):
        return self._server.server_address[1]

    @property
    def connections(self):
        return self._server.connections

    @property
    def url(self):
        return f"http://127.0.0.1:{self.port}"

    def start(self):
        self._thread.start()

    def stop(self):
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)

    def _make_handler(self):
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def _read_body(self):
                length = int(self.headers.get("Content-Length") or 0)
                return self.rfile.read(length) if length else b""

            def _send(self, status, payload):
                body = payload if isinstance(payload, bytes) else json.dumps(payload).encode("utf-8")
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                self._route("GET")

            def do_POST(self):
                self._route("POST")

            def do_DELETE(self):
                self._route("DELETE")

            def _route(self, method):
                body = self._read_body()
                outer.requests.append(
                    {
                        "method": method,
                        "path": self.path,
                        "headers": {k: v for k, v in self.headers.items()},
                        "body": body,
                    }
                )
                if outer.behavior is not None:
                    override = outer.behavior(method, self.path, body)
                    if override is not None:
                        status, payload = override
                        self._send(status, payload)
                        return
                path = self.path
                if method == "GET" and path == "/v1/snapshots":
                    self._send(200, outer.snapshots)
                    return
                if method == "GET" and path.startswith("/v1/snapshots/"):
                    rest = path[len("/v1/snapshots/") :]
                    if rest.endswith("/info"):
                        tag = urllib.parse.unquote(rest[: -len("/info")])
                        if tag in outer.info_404_tags or tag not in outer.snapshot_by_tag:
                            self._send(404, {"error": "snapshot not found"})
                        else:
                            self._send(200, outer.snapshot_by_tag[tag])
                    else:
                        tag = urllib.parse.unquote(rest)
                        if tag in outer.snapshot_by_tag:
                            self._send(200, outer.snapshot_by_tag[tag])
                        else:
                            self._send(404, {"error": "snapshot not found"})
                    return
                if method == "GET" and path == "/v1/sandboxes":
                    self._send(200, outer.sandboxes)
                    return
                if method == "POST" and path == "/v1/sandboxes":
                    request = json.loads(body or b"{}")
                    created = []
                    for _ in range(int(request.get("n", 1))):
                        outer._next_id += 1
                        info = {
                            "id": f"sb-{outer._next_id}",
                            "snapshot_tag": request.get("snapshot_tag", ""),
                            "guest_addr": outer.guest_addr,
                            "created_at_unix": 1700000000,
                        }
                        outer.sandboxes.append(info)
                        created.append(info)
                    self._send(200, created)
                    return
                if method == "POST" and path.startswith("/v1/sandboxes/") and path.endswith("/ping"):
                    self._send(200, {"pong": True})
                    return
                if method == "DELETE" and path.startswith("/v1/sandboxes/"):
                    sid = urllib.parse.unquote(path[len("/v1/sandboxes/") :])
                    if sid in outer.delete_404_ids:
                        self._send(404, {"error": "sandbox not found"})
                    else:
                        outer.sandboxes = [s for s in outer.sandboxes if s["id"] != sid]
                        self._send(200, {"ok": True})
                    return
                self._send(404, {"error": "not found"})

        return Handler


# ---------------------------------------------------------------------------
# forkd guest NDJSON fake
# ---------------------------------------------------------------------------


class FakeNdjsonGuestServer:
    """Minimal forkd guest speaking newline-delimited JSON."""

    def __init__(
        self,
        *,
        exec_stdout=b"hello\n",
        exec_stderr=b"",
        exec_exit=0,
        exec_timed_out=False,
        eval_output=b"eval out\n",
        eval_status=0,
        ls_entries=None,
        find_matches=None,
        grep_matches=None,
        read_data=b"hi",
        read_truncated=False,
        read_total=None,
    ):
        self.exec_stdout = exec_stdout
        self.exec_stderr = exec_stderr
        self.exec_exit = exec_exit
        self.exec_timed_out = exec_timed_out
        self.eval_output = eval_output
        self.eval_status = eval_status
        self.ls_entries = ls_entries if ls_entries is not None else [
            {"name": "a.txt", "is_dir": False, "size": 3}
        ]
        self.find_matches = find_matches if find_matches is not None else ["a.txt"]
        self.grep_matches = grep_matches if grep_matches is not None else [
            {"path": "a.txt", "line": 1, "column": 1, "text": "hi"}
        ]
        self.read_data = read_data
        self.read_truncated = read_truncated
        self.read_total = read_total
        self.error_response = None  # str: respond {"error": ...} to the next request
        self.fail_next = None  # str: respond {"error": ...} once, then clear
        self.oversize_response = False  # respond with a >1 MiB line
        self.ping_response = {"pong": True}  # scriptable ping terminal line
        self.exec_null_exit_code = False  # send "exit_code": null on the exec terminal line
        self.received = []
        self.stream_inputs = []
        self.stop_requested = False
        self.connections = 0
        self._server = _ThreadedTcpServer(("127.0.0.1", 0), self._make_handler())
        self._thread = threading.Thread(target=self._server.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)

    @property
    def port(self):
        return self._server.server_address[1]

    @property
    def address(self):
        return f"127.0.0.1:{self.port}"

    def start(self):
        self._thread.start()

    def stop(self):
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)

    def _make_handler(self):
        outer = self

        class Handler(socketserver.BaseRequestHandler):
            def handle(self):
                outer.connections += 1
                rfile = self.request.makefile("rb")
                wfile = self.request.makefile("wb")

                def send(obj):
                    wfile.write(json.dumps(obj, separators=(",", ":")).encode("utf-8") + b"\n")
                    wfile.flush()

                line = rfile.readline(1024 * 1024 + 1)
                if not line:
                    return
                value = json.loads(line.decode("utf-8"))
                outer.received.append(value)
                action = value.get("action")
                if outer.fail_next is not None:
                    message = outer.fail_next
                    outer.fail_next = None
                    send({"error": message})
                    return
                if outer.error_response is not None:
                    send({"error": outer.error_response})
                    return
                if outer.oversize_response:
                    wfile.write(b"x" * (1024 * 1024 + 16) + b"\n")
                    wfile.flush()
                    return
                if action == "ping":
                    send(outer.ping_response)
                elif action == "exec":
                    # First a non-terminal progress line, then the terminal
                    # line carrying exit_code/stdout/stderr (the guest puts
                    # the exec result fields on the terminal line).
                    send({"progress": "starting"})
                    if outer.exec_null_exit_code:
                        send(
                            {
                                "exit_code": None,
                                "timed_out": outer.exec_timed_out,
                                "stdout": list(outer.exec_stdout),
                                "stderr": list(outer.exec_stderr),
                            }
                        )
                    else:
                        send(
                            {
                                "exit_code": outer.exec_exit,
                                "timed_out": outer.exec_timed_out,
                                "stdout": list(outer.exec_stdout),
                                "stderr": list(outer.exec_stderr),
                            }
                        )
                elif action == "eval":
                    send(
                        {
                            "output": list(outer.eval_output),
                            "status": outer.eval_status,
                            "timed_out": False,
                        }
                    )
                elif action == "ls":
                    send({"entries": outer.ls_entries, "truncated": False})
                elif action == "find":
                    send({"matches": outer.find_matches, "truncated": False})
                elif action == "grep":
                    send({"matches": outer.grep_matches, "truncated": False})
                elif action == "read":
                    result = {
                        "data": list(outer.read_data),
                        "truncated": outer.read_truncated,
                    }
                    if outer.read_total is not None:
                        result["total_bytes"] = outer.read_total
                    send(result)
                elif action == "write":
                    send({"bytes_written": len(value.get("data", []))})
                elif action == "stream":
                    send({"event": "started"})
                    while True:
                        session_line = rfile.readline(1024 * 1024 + 1)
                        if not session_line:
                            break
                        session_value = json.loads(session_line.decode("utf-8"))
                        outer.stream_inputs.append(session_value)
                        if "in" in session_value:
                            send({"stdout": list(session_value["in"].encode("utf-8"))})
                        elif session_value.get("action") == "stop":
                            outer.stop_requested = True
                            send({"exit_code": 130})
                            break

        return Handler


# ---------------------------------------------------------------------------
# ZBRT v1 frame fake
# ---------------------------------------------------------------------------


class FakeZbrtServer:
    """Minimal ZBRT v1 frame server.

    Execute behavior is scriptable: argv[0] == "echo" streams b"hello\\n" on
    stdout then exits 0; any other argv (the eval mapping) streams
    b"eval out\\n" then exits 0. A stderr line is emitted when
    ``exec_stderr`` is set for echo-style commands.
    """

    def __init__(self, *, exec_stderr=b""):
        self.exec_stderr = exec_stderr
        self.fail_execute_message = None  # str -> Error frame for Execute
        self.fail_fs_message = None  # str -> Error frame for Fs
        self.fail_next = None  # str -> Error frame for the next request, then clear
        self.hold_execute_open = False  # hold non-echo Execute until Cancel
        self.connections = 0
        self.received_fs_ops = []
        self.received_cancels = []
        self.received_executes = []
        self._held_request_id = None
        self._server = _ThreadedTcpServer(("127.0.0.1", 0), self._make_handler())
        self._thread = threading.Thread(target=self._server.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)

    @property
    def port(self):
        return self._server.server_address[1]

    @property
    def address(self):
        return f"127.0.0.1:{self.port}"

    def start(self):
        self._thread.start()

    def stop(self):
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)

    def _fs_result(self, op, path, data_json):
        if op == 1:
            return {"entries": [{"name": "a.txt", "is_dir": False, "size": 3}], "truncated": False}
        if op == 2:
            return {"matches": ["a.txt"], "truncated": False}
        if op == 3:
            return {
                "matches": [{"path": "a.txt", "line": 1, "column": 1, "text": "hi"}],
                "truncated": False,
            }
        if op == 4:
            return {"data": list(b"hi"), "truncated": False, "total_bytes": 2}
        if op == 5:
            return {"bytes_written": len(data_json.get("data", []))}
        raise AssertionError(f"unexpected fs op {op}")

    def _make_handler(self):
        outer = self

        class Handler(socketserver.BaseRequestHandler):
            def handle(self):
                outer.connections += 1
                try:
                    self._serve()
                except (ConnectionResetError, ConnectionAbortedError, BrokenPipeError, OSError):
                    # The test client may close with unread frames pending;
                    # that is normal teardown, not a test failure.
                    pass

            def _serve(self):
                rfile = self.request.makefile("rb")
                wfile = self.request.makefile("wb")
                while True:
                    frame = read_frame(rfile)
                    if frame is None:
                        return
                    kind, request_id, payload = frame
                    if outer.fail_next is not None:
                        message = outer.fail_next
                        outer.fail_next = None
                        error_payload = (
                            struct.pack(">I", 1) + struct.pack(">I", len(message)) + message.encode("utf-8")
                        )
                        write_frame(wfile, KIND_ERROR, request_id, error_payload)
                        continue
                    if kind == 1:  # Hello
                        write_frame(wfile, KIND_HELLO_ACK, request_id, payload)
                    elif kind == KIND_EXECUTE:
                        argv, cwd, stdin, timeout_ms = parse_execute(payload)
                        outer.received_executes.append((argv, cwd, stdin, timeout_ms))
                        if outer.fail_execute_message is not None:
                            error_payload = struct.pack(">I", 1) + struct.pack(
                                ">I", len(outer.fail_execute_message)
                            ) + outer.fail_execute_message.encode("utf-8")
                            write_frame(wfile, KIND_ERROR, request_id, error_payload)
                            continue
                        if outer.hold_execute_open and not (argv and argv[0] == "echo"):
                            # Simulate a long-running stream: hold the turn
                            # open until a Cancel arrives.
                            outer._held_request_id = request_id
                            continue
                        data = b"hello\n" if argv and argv[0] == "echo" else b"eval out\n"
                        write_frame(wfile, KIND_OUTPUT, request_id, bytes([0]) + struct.pack(">I", len(data)) + data)
                        if argv and argv[0] == "echo" and outer.exec_stderr:
                            err = outer.exec_stderr
                            write_frame(wfile, KIND_OUTPUT, request_id, bytes([1]) + struct.pack(">I", len(err)) + err)
                        exit_payload = struct.pack(">i", 0) + b"\x00"
                        write_frame(wfile, KIND_EXIT, request_id, exit_payload)
                    elif kind == 6:  # Cancel
                        outer.received_cancels.append(payload)
                        write_frame(wfile, KIND_CANCEL_ACK, request_id, b"")
                        if outer._held_request_id is not None:
                            exit_payload = struct.pack(">i", -1) + b"\x00"
                            write_frame(wfile, KIND_EXIT, outer._held_request_id, exit_payload)
                            outer._held_request_id = None
                    elif kind == 8:  # Fs
                        op = payload[0]
                        (path_len,) = struct.unpack_from(">I", payload, 1)
                        path = payload[5 : 5 + path_len].decode("utf-8")
                        (data_len,) = struct.unpack_from(">I", payload, 5 + path_len)
                        raw_data = payload[9 + path_len : 9 + path_len + data_len]
                        try:
                            data_json = json.loads(raw_data.decode("utf-8"))
                        except (UnicodeDecodeError, json.JSONDecodeError):
                            data_json = {}
                        outer.received_fs_ops.append((op, path, data_json))
                        if outer.fail_fs_message is not None:
                            message = outer.fail_fs_message
                            error_payload = (
                                struct.pack(">I", 1) + struct.pack(">I", len(message)) + message.encode("utf-8")
                            )
                            write_frame(wfile, KIND_ERROR, request_id, error_payload)
                            continue
                        result = outer._fs_result(op, path, data_json)
                        result_payload = json.dumps(result, separators=(",", ":")).encode("utf-8")
                        write_frame(wfile, KIND_FS_RESULT, request_id, result_payload)
                    elif kind == 10:  # Health
                        health = bytes([1]) + bytes([1]) + struct.pack(">I", 5) + b"ready"
                        write_frame(wfile, KIND_HEALTH_ACK, request_id, health)
                    else:
                        message = b"unsupported request kind"
                        error_payload = (
                            struct.pack(">I", 1) + struct.pack(">I", len(message)) + message
                        )
                        write_frame(wfile, KIND_ERROR, request_id, error_payload)

        return Handler
