"""RFB SDK error hierarchy (UNIFIED_API.md section 7)."""


class RfbError(Exception):
    """Base class for every error raised by the rfb SDK."""


class TransportError(RfbError):
    """Connection / read / write failure or timeout at the transport level."""


class HttpStatusError(RfbError):
    """The forkd controller returned a non-2xx HTTP status."""

    def __init__(self, status: int, message: str):
        super().__init__(f"forkd returned {status}: {message}")
        self.status = status
        self.message = message


class DecodeError(RfbError):
    """A response or frame could not be decoded (including strict-codec rejections)."""


class RemoteError(RfbError):
    """The remote peer reported an error (guest error line, ZBRT Error frame, forkd error field)."""


class ValidationError(RfbError):
    """Local pre-send validation failed (fail closed)."""
