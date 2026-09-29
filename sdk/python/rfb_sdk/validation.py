"""Local pre-send validation rules (PROTOCOL.md section 2.3, rfb/src/guest/limits.rs).

Every check fails closed: the request is rejected with ValidationError before
any bytes hit the wire.
"""

import math

from .errors import ValidationError

MAX_GUEST_PATH_BYTES = 4096
MAX_GUEST_PATTERN_BYTES = 1024
MAX_GUEST_RESULTS = 1000
MAX_GUEST_RESULT_BYTES = 50 * 1024
MAX_GUEST_CODE_BYTES = 1024 * 1024
# ZBRT v1 caps: argc fits one header byte, payloads are u32-bounded.
MAX_ZBRT_ARGC = 255
MAX_ZBRT_PAYLOAD_BYTES = 16 * 1024 * 1024


def _byte_len(value: str) -> int:
    return len(value.encode("utf-8"))


def _invalid_path(path: str, file_path: bool) -> bool:
    """Shared fail-closed checks; file_path mode additionally rejects drive letters."""
    return (
        not path
        or _byte_len(path) > MAX_GUEST_PATH_BYTES
        or "\x00" in path
        or "\\" in path
        or any(segment == ".." for segment in path.split("/"))
        or (
            path.startswith("/")
            and not file_path
            and not (path == "/workspace" or path.startswith("/workspace/"))
        )
        or (file_path and len(path) >= 2 and path[1] == ":")
    )


def validate_fs_path(path: str) -> None:
    """Structured fs RPC path (ls/find/grep): relative or /workspace-prefixed."""
    if _invalid_path(path, False):
        raise ValidationError(
            "invalid guest fs path: must be a non-empty, relative, non-escaping guest path"
        )


def validate_guest_file_path(path: str) -> None:
    """File path (read/write and eval/stream cwd): relative or absolute, no host-isms."""
    if _invalid_path(path, True):
        raise ValidationError("invalid guest file path: must be a non-empty, non-escaping guest path")


def validate_guest_cwd(cwd: str) -> None:
    validate_guest_file_path(cwd)


def validate_pattern(pattern: str) -> None:
    if not pattern or "\x00" in pattern:
        raise ValidationError("invalid guest pattern: must be non-empty and NUL-free")
    if _byte_len(pattern) > MAX_GUEST_PATTERN_BYTES:
        raise ValidationError("guest pattern exceeds 1024 bytes")


def validate_payload_size(value: int, max_value: int) -> None:
    if value > max_value:
        raise ValidationError(f"guest payload exceeds {max_value} bytes")


def validate_eval_code(code: str) -> None:
    if not code.strip():
        raise ValidationError("eval code must not be empty")
    if _byte_len(code) > MAX_GUEST_CODE_BYTES:
        raise ValidationError("eval code exceeds 1 MiB")


def validate_timeout(timeout_s, what: str = "timeout") -> None:
    """Reject non-numeric, non-finite, or non-positive timeouts (fail closed)."""
    if not isinstance(timeout_s, (int, float)) or isinstance(timeout_s, bool):
        raise ValidationError(f"{what} must be a number of seconds")
    if not math.isfinite(timeout_s) or timeout_s <= 0:
        raise ValidationError(f"{what} must be a finite number greater than zero")


def validate_eval_timeout(timeout_s) -> None:
    if timeout_s is None:
        return
    validate_timeout(timeout_s, "eval timeout")


def validate_zbrt_args(args) -> None:
    """ZBRT v1 encodes argc in one byte; reject locally instead of leaking a
    ValueError after the connection is already open."""
    if len(args) > MAX_ZBRT_ARGC:
        raise ValidationError(f"argv exceeds the {MAX_ZBRT_ARGC}-argument ZBRT limit")


_ID_CHARS = frozenset(
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_"
)


def _validate_identifier(value: str, what: str) -> None:
    if (
        not isinstance(value, str)
        or not value
        or len(value) > 128
        or not _ID_CHARS.issuperset(value)
    ):
        raise ValidationError(f"invalid {what}: must be non-empty, <= 128 chars, [A-Za-z0-9_-] only")


def validate_sandbox_id(sandbox_id: str) -> None:
    _validate_identifier(sandbox_id, "forkd sandbox id")


def validate_argv(args) -> None:
    if not isinstance(args, (list, tuple)) or len(args) == 0:
        raise ValidationError("argv must be a non-empty list of strings")
    if not all(isinstance(a, str) for a in args):
        raise ValidationError("argv entries must be strings")


def validate_transport(transport: str) -> None:
    if transport not in ("ndjson", "zbrt"):
        raise ValidationError("transport must be 'ndjson' or 'zbrt'")
