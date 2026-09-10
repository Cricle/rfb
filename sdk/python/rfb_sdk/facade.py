"""RfbClient / Sandbox / GuestStream public facade (UNIFIED_API.md sections 2-5)."""

import os
import time
from typing import Any, Optional

from ._forkd import DEFAULT_BASE_URL, DEFAULT_TIMEOUT_S, _ForkdController
from ._guest import _GuestNdjsonClient, _as_bytes, _first_of
from ._zbrt import (
    FS_OP_FIND,
    FS_OP_GREP,
    FS_OP_LS,
    FS_OP_READ,
    FS_OP_WRITE,
    _ZbrtGuestClient,
)
from .errors import DecodeError, RemoteError, TransportError, ValidationError
from .models import DirEntry, ExecResult, FileRead, GrepMatch, SandboxInfo, Snapshot
from .validation import (
    MAX_GUEST_RESULTS,
    MAX_GUEST_RESULT_BYTES,
    validate_argv,
    validate_eval_code,
    validate_eval_timeout,
    validate_fs_path,
    validate_guest_cwd,
    validate_guest_file_path,
    validate_pattern,
    validate_payload_size,
    validate_sandbox_id,
    validate_transport,
)

DEFAULT_EXEC_TIMEOUT_S = 60.0
DEFAULT_WAIT_TIMEOUT_S = 60
_WAIT_POLL_INTERVAL_S = 0.1


def _require_list(value: dict, key: str, what: str) -> list:
    items = value.get(key)
    if not isinstance(items, list):
        raise DecodeError(f"{what} result is missing '{key}'")
    return items


def _entries(value: dict) -> list:
    result = []
    for entry in _require_list(value, "entries", "ls"):
        if not isinstance(entry, dict):
            raise DecodeError("ls entry must be a JSON object")
        result.append(
            DirEntry(
                name=entry.get("name", ""),
                is_dir=bool(entry.get("is_dir", False)),
                size=entry.get("size"),
            )
        )
    return result


def _grep_matches(value: dict) -> list:
    result = []
    for match in _require_list(value, "matches", "grep"):
        if not isinstance(match, dict):
            raise DecodeError("grep match must be a JSON object")
        result.append(
            GrepMatch(
                path=match.get("path", ""),
                line=match.get("line"),
                column=match.get("column"),
                text=match.get("text", ""),
            )
        )
    return result


def _zbrt_exec_result(t: tuple) -> ExecResult:
    code, stdout, stderr = t
    return ExecResult(exit_code=code, stdout=stdout, stderr=stderr, timed_out=False)


class RfbClient:
    """The single public client of the RFB SDK.

    Mirrors the Rust reference implementation ``rfb::client::RfbClient``.
    """

    def __init__(
        self,
        base_url: Optional[str] = None,
        token: Optional[str] = None,
        timeout_s: float = DEFAULT_TIMEOUT_S,
    ):
        if base_url is None:
            base_url = os.environ.get("FORKD_URL") or DEFAULT_BASE_URL
        if token is None:
            token = os.environ.get("FORKD_TOKEN")
        self.base_url = base_url
        self.timeout_s = timeout_s
        self._controller = _ForkdController(base_url, token, timeout_s)

    # -- snapshots ---------------------------------------------------------

    def list_snapshots(self) -> list:
        return [Snapshot.from_json(item) for item in self._controller.list_snapshots()]

    def snapshot(self, tag: str) -> Optional[Snapshot]:
        value = self._controller.snapshot(tag)
        return None if value is None else Snapshot.from_json(value)

    def wait_snapshot(self, tag: str, timeout_s: int = DEFAULT_WAIT_TIMEOUT_S) -> Snapshot:
        deadline = time.monotonic() + timeout_s
        while True:
            for item in self._controller.list_snapshots():
                if item.get("tag") != tag:
                    continue
                status = str(item.get("status", "")).lower()
                if status == "ready" and bool(item.get("bootable", False)):
                    return Snapshot.from_json(item)
                if status == "failed":
                    raise RemoteError(f"snapshot {tag!r} failed")
            if time.monotonic() >= deadline:
                raise TransportError(f"timed out waiting for snapshot {tag!r}")
            time.sleep(_WAIT_POLL_INTERVAL_S)

    # -- sandboxes ----------------------------------------------------------

    def create_sandbox(
        self,
        snapshot_tag: str,
        n: int = 1,
        per_child_netns: bool = False,
        memory_limit_mib: Optional[int] = None,
        prewarm: bool = False,
        live_fork: bool = False,
        hugepages: bool = False,
        transport: str = "ndjson",
    ) -> list:
        validate_transport(transport)
        request = {
            "snapshot_tag": snapshot_tag,
            "n": n,
            "per_child_netns": per_child_netns,
            "memory_limit_mib": memory_limit_mib,
            "prewarm": prewarm,
            "live_fork": live_fork,
            "hugepages": hugepages,
        }
        return [
            Sandbox(SandboxInfo.from_json(item), self, transport)
            for item in self._controller.create_sandboxes(request)
        ]

    def list_sandboxes(self) -> list:
        return [
            Sandbox(SandboxInfo.from_json(item), self)
            for item in self._controller.list_sandboxes()
        ]

    def connect(self, sandbox_or_id, transport: Optional[str] = None) -> "Sandbox":
        if isinstance(sandbox_or_id, Sandbox):
            # Rust attach semantics (ConnectTarget for &Sandbox): attach the
            # facade as-is; only an explicitly passed transport overrides it.
            if transport is None:
                return Sandbox(sandbox_or_id.info, self, sandbox_or_id.transport)
            validate_transport(transport)
            return Sandbox(sandbox_or_id.info, self, transport)
        if transport is None:
            transport = "ndjson"
        validate_transport(transport)
        sandbox_id = sandbox_or_id
        validate_sandbox_id(sandbox_id)
        for item in self._controller.list_sandboxes():
            if item.get("id") == sandbox_id:
                return Sandbox(SandboxInfo.from_json(item), self, transport)
        raise RemoteError(f"sandbox not found: {sandbox_id}")

    def ping_sandbox(self, sandbox_id: str) -> Any:
        return self._controller.ping_sandbox(sandbox_id)

    def delete_sandbox(self, sandbox_id: str) -> None:
        self._controller.delete_sandbox(sandbox_id)


class Sandbox:
    """Facade over one sandbox's guest, in front of the internal transports."""

    def __init__(self, info: SandboxInfo, client: RfbClient, transport: str = "ndjson"):
        validate_transport(transport)
        if not isinstance(info, SandboxInfo):
            raise TypeError("info must be a SandboxInfo")
        self.info = info
        self._client = client
        self._transport = transport

    @property
    def id(self) -> str:
        return self.info.id

    @property
    def snapshot_tag(self) -> str:
        return self.info.snapshot_tag

    @property
    def guest_addr(self) -> str:
        return self.info.guest_addr

    @property
    def transport(self) -> str:
        """The guest transport this facade uses (Rust ``Sandbox::transport``)."""
        return self._transport

    @property
    def created_at_unix(self) -> Optional[int]:
        return self.info.created_at_unix

    def _guest(self):
        timeout_s = self._client.timeout_s
        if self._transport == "zbrt":
            return _ZbrtGuestClient(self.guest_addr, timeout_s)
        return _GuestNdjsonClient(self.guest_addr, timeout_s)

    def _fs_call(self, op: int, action: str, path: str, args: dict) -> dict:
        """Run one filesystem RPC over the active transport.

        ZBRT carries ``args`` as-is; the NDJSON action dict drops None values
        (absent keys on the wire) to mirror the historical request shape.
        """
        guest = self._guest()
        if self._transport == "zbrt":
            return guest.fs_op(op, path, args)
        request = {"action": action, "path": path}
        request.update((k, v) for k, v in args.items() if v is not None)
        return guest.tool(request)

    # -- health --------------------------------------------------------------

    def ping(self) -> bool:
        guest = self._guest()
        if self._transport == "zbrt":
            return guest.ping()
        # Rust baseline (ndjson::ping_healthy): healthy only when the pong
        # flag is present and true.
        return guest.ping().get("pong") is True

    # -- exec / eval ---------------------------------------------------------

    def exec(self, args, cwd: str = "/", timeout_s: float = DEFAULT_EXEC_TIMEOUT_S, stdin=b""):
        validate_argv(args)
        validate_guest_cwd(cwd)
        stdin = stdin.encode("utf-8") if isinstance(stdin, str) else bytes(stdin)
        guest = self._guest()
        if self._transport == "zbrt":
            return _zbrt_exec_result(guest.exec(args, cwd, timeout_s, stdin))
        value = guest.exec(cwd, args, timeout_s)
        # Rust baseline (ndjson::exec_result): a missing or non-integer
        # exit_code falls back to -1.
        raw = value.get("exit_code")
        exit_code = int(raw) if isinstance(raw, int) and not isinstance(raw, bool) else -1
        return ExecResult(
            exit_code=exit_code,
            # Legacy guests emit out/err instead of stdout/stderr; accept both.
            stdout=_as_bytes(_first_of(value, "stdout", "out")),
            stderr=_as_bytes(_first_of(value, "stderr", "err")),
            timed_out=bool(value.get("timed_out", False)),
        )

    def eval(self, code: str, cwd: Optional[str] = None, timeout_s: Optional[float] = None):
        validate_eval_code(code)
        if cwd is not None:
            validate_guest_cwd(cwd)
        validate_eval_timeout(timeout_s)
        guest = self._guest()
        if self._transport == "zbrt":
            return _zbrt_exec_result(guest.eval(code, cwd, timeout_s))
        value = guest.eval(code, cwd, timeout_s)
        status = value.get("status")
        return ExecResult(
            exit_code=int(status) if status is not None else 0,
            # Legacy guests emit out instead of output; accept both.
            stdout=_as_bytes(_first_of(value, "output", "out")),
            stderr=b"",
            timed_out=bool(value.get("timed_out", False)),
        )

    # -- filesystem ----------------------------------------------------------

    def ls(self, path: str = ".") -> list:
        validate_fs_path(path)
        return _entries(
            self._fs_call(FS_OP_LS, "ls", path, {"max_results": MAX_GUEST_RESULTS})
        )

    def find(self, path: str = ".", *, pattern: str) -> list:
        validate_fs_path(path)
        validate_pattern(pattern)
        value = self._fs_call(
            FS_OP_FIND, "find", path, {"pattern": pattern, "max_results": MAX_GUEST_RESULTS}
        )
        return _require_list(value, "matches", "find")

    def grep(self, path: str = ".", *, pattern: str) -> list:
        validate_fs_path(path)
        validate_pattern(pattern)
        value = self._fs_call(
            FS_OP_GREP,
            "grep",
            path,
            {
                "pattern": pattern,
                "max_results": MAX_GUEST_RESULTS,
                "max_bytes": MAX_GUEST_RESULT_BYTES,
            },
        )
        return _grep_matches(value)

    def read(self, path: str, offset: Optional[int] = None, max_bytes: Optional[int] = None) -> FileRead:
        validate_guest_file_path(path)
        if max_bytes is not None and (max_bytes <= 0 or max_bytes > MAX_GUEST_RESULT_BYTES):
            raise ValidationError(
                f"read max_bytes must be > 0 and <= {MAX_GUEST_RESULT_BYTES}"
            )
        value = self._fs_call(
            FS_OP_READ, "read", path, {"offset": offset, "max_bytes": max_bytes}
        )
        return FileRead(
            data=_as_bytes(value.get("data")),
            truncated=bool(value.get("truncated", False)),
            total_bytes=value.get("total_bytes"),
        )

    def write(self, path: str, data, append: bool = False, mode: Optional[int] = None) -> int:
        validate_guest_file_path(path)
        data = data.encode("utf-8") if isinstance(data, str) else bytes(data)
        validate_payload_size(len(data), MAX_GUEST_RESULT_BYTES)
        value = self._fs_call(
            FS_OP_WRITE,
            "write",
            path,
            {"data": list(data), "append": append, "mode": mode},
        )
        if "bytes_written" not in value:
            raise DecodeError("write result is missing 'bytes_written'")
        return int(value["bytes_written"])

    # -- stream / lifecycle ---------------------------------------------------

    def stream(self, args, cwd: Optional[str] = None, pty: Optional[bool] = None, env: Optional[dict] = None) -> "GuestStream":
        validate_argv(args)
        if cwd is not None:
            validate_guest_cwd(cwd)
        guest = self._guest()
        if self._transport == "zbrt":
            # Rust baseline (client/facade.rs GuestOps::stream, ZBRT arm):
            # pty / env are fail-closed rejected before any frame is sent.
            if pty is True:
                raise ValidationError(
                    "pty is not supported over the ZBRT transport"
                )
            if isinstance(env, dict) and env:
                raise ValidationError(
                    "env is not supported over the ZBRT transport"
                )
            inner = guest.open_stream(args, cwd)
        else:
            inner = guest.stream(args, cwd, pty, env)
        return GuestStream(inner)

    def delete(self) -> None:
        self._client.delete_sandbox(self.id)


class GuestStream:
    """Interactive guest stream (UNIFIED_API.md section 5)."""

    def __init__(self, _inner):
        self._inner = _inner

    def next_event(self):
        """Next StreamEvent, or None once the stream closed cleanly."""
        return self._inner.next_event()

    def send_input(self, text: str) -> None:
        self._inner.send_input(text)

    def stop(self) -> None:
        """Idempotently ask the guest to terminate the stream."""
        self._inner.stop()
