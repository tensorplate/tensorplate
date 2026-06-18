"""Typed exceptions and error codes raised by the TensorPlate SDK."""

from __future__ import annotations

from enum import Enum


class ErrorCode(str, Enum):
    """Stable serving error codes shared with the runtime, agent, and CLI.

    Mirrors the ``code`` enum in the v0.1 serving and error schemas. The
    string values are the wire form; the numeric C++ enum is not part of
    the protocol.
    """

    CONFIG_INVALID = "config_invalid"
    LOAD_FAILED = "load_failed"
    NOT_READY = "not_ready"
    SHAPE_MISMATCH = "shape_mismatch"
    UNSUPPORTED = "unsupported"
    OOM_ERROR = "oom_error"
    TIMEOUT = "timeout"
    INFERENCE_FAILED = "inference_failed"
    INTERNAL = "internal"


class TensorPlateError(Exception):
    """Base class for every error raised by the TensorPlate SDK.

    Transport, protocol, and serving-failure subtypes derive from this
    base so callers can catch the entire SDK error surface with a single
    ``except`` clause.
    """


class EndpointResolutionError(TensorPlateError):
    """Raised when the serving endpoint cannot be resolved or canonicalized."""


class TransportError(TensorPlateError):
    """Raised when the serving endpoint cannot be reached or the HTTP exchange fails."""


class RequestTimeoutError(TransportError):
    """Raised when a request to the serving endpoint exceeds its timeout."""


class ProtocolError(TensorPlateError):
    """Raised when a response is not valid JSON or violates the serving envelope."""


class UnsupportedSchemaVersionError(ProtocolError):
    """Raised when a response declares a ``schema_version`` the SDK does not support."""

    def __init__(self, received: str | None, supported: str) -> None:
        self.received = received
        self.supported = supported
        got = received if received is not None else "<missing>"
        super().__init__(
            f"unsupported serving schema_version {got!r}; this SDK supports {supported!r}"
        )


class ServingError(TensorPlateError):
    """Raised when the serving worker returns a typed ``failure`` envelope.

    Carries the wire ``code``, human-readable ``message``, optional
    ``context``, and the ``request_id`` the worker echoed so failures can
    be correlated with serving logs.
    """

    def __init__(
        self,
        code: ErrorCode,
        message: str,
        *,
        context: str | None = None,
        request_id: str | None = None,
    ) -> None:
        self.code = code
        self.message = message
        self.context = context
        self.request_id = request_id
        detail = f"[{code.value}] {message}"
        if context:
            detail = f"{detail} (context: {context})"
        super().__init__(detail)
