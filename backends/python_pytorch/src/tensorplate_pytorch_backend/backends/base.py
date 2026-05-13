"""Backend implementation interface for the sidecar runner.

The runner is schema-only: it parses incoming frames, dispatches the
operation, and serializes the response. The actual model load /
inference is delegated to a ``Backend`` implementation registered for
the model's `backend_hint`.

V01-E05-F04 ships only the fixture backend (zero dependencies; safe for
host CI). The TorchScript / SmolVLA backend lands in V01-E05-F05.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Protocol


class BackendError(Exception):
    """Raised by backend implementations to surface a typed sidecar error.

    The ``code`` matches a `tensorplate::Error::Code` snake_case name
    (see ``tensorplate_pytorch_backend.protocol``).
    """

    def __init__(self, code: str, message: str, *, context: str | None = None) -> None:
        super().__init__(message)
        self.code = code
        self.code_message = message
        self.context = context


@dataclass(slots=True)
class NamedTensor:
    """One named tensor crossing the sidecar boundary.

    `tensor` carries the schema-defined metadata (dtype, shape,
    optional strides/byte_offset/byte_size) verbatim. Raw bytes live in
    the frame's payload region; the runner is responsible for slicing
    the payload by ``payload_offset`` / ``payload_length`` recorded in
    the frame header before handing it to a backend.
    """

    name: str
    tensor: dict[str, Any]
    payload: bytes


class Backend(Protocol):
    """Lifecycle interface a sidecar backend implements.

    The runner serializes lifecycle calls per session, so backends do
    not need internal synchronization. Backends should raise
    :class:`BackendError` for any typed failure; uncaught exceptions
    are converted to ``Error::Code::Internal`` by the runner.
    """

    def load(self, model_spec: dict[str, Any]) -> None: ...

    def prime(self) -> None: ...

    def infer(self, inputs: list[NamedTensor]) -> list[NamedTensor]: ...

    def infer_async(self, inputs: list[NamedTensor]) -> list[NamedTensor]: ...

    def cancel(self, request_id: str) -> None: ...

    def unload(self) -> None: ...

    @property
    def name(self) -> str: ...


__all__ = ["Backend", "BackendError", "NamedTensor"]
