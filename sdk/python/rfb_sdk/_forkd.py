"""Internal forkd controller HTTP/JSON client.

Mirrors ``rfb/src/forkd/controller.rs``. This module is NOT part of the
public API.
"""

import http.client
import json
import threading
import urllib.parse

from .errors import DecodeError, HttpStatusError, TransportError, ValidationError
from .validation import validate_sandbox_id

DEFAULT_BASE_URL = "http://127.0.0.1:8889"
DEFAULT_TIMEOUT_S = 10.0
# create 的快照恢复（restore + resume）实测可超 10s——provider 侧同款结论
# （forkd/provider.rs: 10s 在 create 上超时会孤儿一个已落地的沙箱）。
CREATE_TIMEOUT_S = 60.0


def _error_message(data: bytes) -> str:
    """Non-2xx message: JSON body ``error`` field, else first 1024 chars."""
    try:
        value = json.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        value = None
    if isinstance(value, dict) and isinstance(value.get("error"), str):
        return value["error"]
    text = data.decode("utf-8", errors="replace")
    if not text:
        return "forkd returned an empty error body"
    return text[:1024]


def _parse_json(data: bytes):
    try:
        return json.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as e:
        raise DecodeError(f"invalid forkd response json: {e}") from e


class _ForkdController:
    """Controller HTTP client with one pooled keep-alive connection."""

    def __init__(self, base_url: str, token, timeout_s: float = DEFAULT_TIMEOUT_S):
        parts = urllib.parse.urlsplit(base_url)
        if parts.scheme not in ("http", "https") or not parts.hostname:
            raise ValidationError(f"invalid forkd base url: {base_url!r}")
        self._use_tls = parts.scheme == "https"
        self._host = parts.hostname
        self._port = parts.port or (443 if self._use_tls else 80)
        self._prefix = parts.path.rstrip("/")
        self._token = token if token else None
        self._timeout_s = timeout_s
        self._conn_cls = (
            http.client.HTTPSConnection if self._use_tls else http.client.HTTPConnection
        )
        self._conn = None
        # UNIFIED_API §2: one client = serialized requests (safe concurrent,
        # never parallel) — the pooled keep-alive connection must not have
        # two threads inside request()/getresponse() at once.
        self._lock = threading.Lock()

    def _connect(self):
        try:
            return self._conn_cls(self._host, self._port, timeout=self._timeout_s)
        except OSError as e:
            raise TransportError(f"forkd request failed: {e}") from e

    def _request(self, method: str, path: str, body=None) -> tuple:
        with self._lock:
            return self._request_locked(method, path, body)

    def _request_locked(self, method: str, path: str, body=None,
                        budget_s: float = None) -> tuple:
        conn = self._conn
        reused = conn is not None and budget_s is None
        if budget_s is not None:
            # 独立预算（create）：临时连接，不复用也不缓存——快照恢复的
            # 预算不能被池化连接上的基础超时钉死。
            conn = self._conn_cls(self._host, self._port, timeout=budget_s)
            self._conn = None
        elif conn is None:
            conn = self._connect()
            self._conn = conn
        headers = {"Accept": "application/json"}
        if body is not None:
            headers["Content-Type"] = "application/json"
        if self._token:
            headers["Authorization"] = f"Bearer {self._token}"
        try:
            conn.request(method, self._prefix + path, body=body, headers=headers)
            resp = conn.getresponse()
            data = resp.read()
            if resp.will_close:
                self._conn = None
                conn.close()
            return resp.status, data
        except (OSError, http.client.HTTPException) as e:
            self._conn = None
            conn.close()
            if budget_s is not None:
                raise TransportError(f"forkd request failed: {e}") from e
            if reused and method in ("GET", "HEAD", "DELETE"):
                # Stale pooled socket (closed by the peer while idle): retry
                # once on a fresh connection, then fail. Non-idempotent
                # requests (POST) are NOT replayed - the peer may have
                # processed them before closing (e.g. an extra sandbox).
                # _request_locked, NOT _request: the lock is already held and
                # threading.Lock is not reentrant (a self-deadlock hung the
                # whole suite when a fake server's keep-alive socket went
                # stale).
                return self._request_locked(method, path, body)
            raise TransportError(f"forkd request failed: {e}") from e

    def _json_request(self, method: str, path: str, body=None, budget_s=None):
        status, data = self._request(method, path, body)
        if not 200 <= status < 300:
            raise HttpStatusError(status, _error_message(data))
        return _parse_json(data)

    def list_snapshots(self) -> list:
        value = self._json_request("GET", "/v1/snapshots")
        if not isinstance(value, list):
            raise DecodeError("snapshot list must be a JSON array")
        return value

    def snapshot(self, tag: str):
        quoted = urllib.parse.quote(tag, safe="")
        status, data = self._request("GET", f"/v1/snapshots/{quoted}/info")
        if status == 404:
            # Fallback to the legacy endpoint; both 404s mean "no snapshot".
            status, data = self._request("GET", f"/v1/snapshots/{quoted}")
            if status == 404:
                return None
        if not 200 <= status < 300:
            raise HttpStatusError(status, _error_message(data))
        value = _parse_json(data)
        if not isinstance(value, dict):
            raise DecodeError("snapshot must be a JSON object")
        return value

    def create_sandboxes(self, request: dict) -> list:
        body = json.dumps(request, separators=(",", ":"))
        value = self._json_request("POST", "/v1/sandboxes", body=body,
                                   budget_s=CREATE_TIMEOUT_S)
        if not isinstance(value, list):
            raise DecodeError("sandbox list must be a JSON array")
        return value

    def list_sandboxes(self) -> list:
        value = self._json_request("GET", "/v1/sandboxes")
        if not isinstance(value, list):
            raise DecodeError("sandbox list must be a JSON array")
        return value

    def ping_sandbox(self, sandbox_id: str):
        validate_sandbox_id(sandbox_id)
        quoted = urllib.parse.quote(sandbox_id, safe="")
        return self._json_request("POST", f"/v1/sandboxes/{quoted}/ping")

    def delete_sandbox(self, sandbox_id: str) -> None:
        validate_sandbox_id(sandbox_id)
        quoted = urllib.parse.quote(sandbox_id, safe="")
        status, data = self._request("DELETE", f"/v1/sandboxes/{quoted}")
        # 2xx or 404 both count as success.
        if status == 404 or 200 <= status < 300:
            return
        raise HttpStatusError(status, _error_message(data))
