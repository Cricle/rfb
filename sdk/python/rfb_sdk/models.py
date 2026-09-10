"""Public result/DTO types (UNIFIED_API.md sections 1 and 6)."""

from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Optional


class StreamEventKind(Enum):
    STARTED = "started"
    STDOUT = "stdout"
    STDERR = "stderr"
    EXIT = "exit"


@dataclass(frozen=True)
class StreamEvent:
    """One event from an interactive guest stream."""

    kind: StreamEventKind
    data: bytes = b""
    code: Optional[int] = None


@dataclass(frozen=True)
class ExecResult:
    """Unified result of exec/eval."""

    exit_code: int
    stdout: bytes = b""
    stderr: bytes = b""
    timed_out: bool = False

    @property
    def stdout_text(self) -> str:
        return (self.stdout or b"").decode("utf-8", errors="replace")

    @property
    def stderr_text(self) -> str:
        return (self.stderr or b"").decode("utf-8", errors="replace")


@dataclass(frozen=True)
class DirEntry:
    name: str
    is_dir: bool = False
    size: Optional[int] = None


@dataclass(frozen=True)
class GrepMatch:
    path: str
    line: Optional[int] = None
    column: Optional[int] = None
    text: str = ""


@dataclass(frozen=True)
class FileRead:
    data: bytes = b""
    truncated: bool = False
    total_bytes: Optional[int] = None


@dataclass
class Snapshot:
    """forkd controller snapshot DTO (PROTOCOL.md section 1.3, serde defaults)."""

    tag: str = ""
    dir: str = ""
    created_at_unix: Optional[int] = None
    branched_from: Optional[str] = None
    pause_ms: Optional[int] = None
    diff_ms: Optional[int] = None
    diff_physical_bytes: Optional[int] = None
    diff_logical_bytes: Optional[int] = None
    warning: Optional[str] = None
    status: str = ""
    bootable: bool = False
    digest: Optional[str] = None
    provenance: Any = None

    @classmethod
    def from_json(cls, value: dict) -> "Snapshot":
        return cls(
            tag=value.get("tag", ""),
            dir=value.get("dir", ""),
            created_at_unix=value.get("created_at_unix"),
            branched_from=value.get("branched_from"),
            pause_ms=value.get("pause_ms"),
            diff_ms=value.get("diff_ms"),
            diff_physical_bytes=value.get("diff_physical_bytes"),
            diff_logical_bytes=value.get("diff_logical_bytes"),
            warning=value.get("warning"),
            status=value.get("status", ""),
            bootable=bool(value.get("bootable", False)),
            digest=value.get("digest"),
            provenance=value.get("provenance"),
        )


@dataclass
class SandboxInfo:
    """forkd controller sandbox DTO (PROTOCOL.md section 1.3, serde defaults)."""

    id: str = ""
    snapshot_tag: str = ""
    netns: Optional[str] = None
    created_at_unix: Optional[int] = None
    guest_addr: str = ""
    memory_limit_mib: Optional[int] = None
    pid: Optional[int] = None
    has_branched: bool = False
    branch_count: int = 0

    @classmethod
    def from_json(cls, value: dict) -> "SandboxInfo":
        return cls(
            id=value.get("id", ""),
            snapshot_tag=value.get("snapshot_tag", ""),
            netns=value.get("netns"),
            created_at_unix=value.get("created_at_unix"),
            guest_addr=value.get("guest_addr", ""),
            memory_limit_mib=value.get("memory_limit_mib"),
            pid=value.get("pid"),
            has_branched=bool(value.get("has_branched", False)),
            branch_count=int(value.get("branch_count", 0)),
        )
