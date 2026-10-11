"""Typed errors of the streaming speech client."""

from __future__ import annotations

from tensorplate.errors import ErrorCode, ProtocolError, TensorPlateError


class SpeechError(TensorPlateError):
    """Base of every error a streaming session raises.

    ``reason`` is the failure reason the server gave, for example
    ``"worker_shutdown"``, and ``code`` the error code paired with it. Both
    are ``None`` when the call ended without a stated cause, which is how a
    refused open ends today. ``grpc_status`` is the name of the call's final
    status, or ``None`` when the client ended the session itself.

    ``admitted`` has three states. ``True``: the server reserved a session
    slot, so the call ran and must not be sent again as if it had not.
    ``False``: the server stated that it reserved none. ``None``: unknown.
    An open that ended with a status and no event is unknown: it may have
    been refused, or admitted and then lost with the connection.
    """

    def __init__(
        self,
        message: str,
        *,
        reason: str | None = None,
        code: ErrorCode | None = None,
        grpc_status: str | None = None,
        admitted: bool | None = None,
    ) -> None:
        self.reason = reason
        self.code = code
        self.grpc_status = grpc_status
        self.admitted = admitted
        super().__init__(message)


class InvalidEventError(SpeechError):
    """The server found an event or its input invalid (``INVALID_ARGUMENT``)."""


class PreconditionFailedError(SpeechError):
    """The deployment does not serve what was asked (``FAILED_PRECONDITION``).

    A mode, language, format or voice it does not declare, another
    deployment or descriptor, a stale generation, or an event the session's
    state does not permit.
    """


class ResourceExhaustedError(SpeechError):
    """A bound was reached (``RESOURCE_EXHAUSTED``).

    The session count, the input credit, memory, or output left unread for
    too long.
    """


class SessionTimeoutError(SpeechError):
    """A timer ended the session (``DEADLINE_EXCEEDED``), or a wait of the client's own did."""


class ServiceUnavailableError(SpeechError):
    """The server cannot serve the session now (``UNAVAILABLE``).

    Admission closed, the worker shutting down or retiring the deployment,
    the backend lost, or an endpoint that cannot be reached. The client does
    not open another session by itself.
    """


class SessionCancelledError(SpeechError):
    """The call was cancelled by something other than this session's ``cancel()``."""


class ServerInternalError(SpeechError):
    """The server failed (``INTERNAL``, or a status no other class stands for)."""


class StreamProtocolError(SpeechError, ProtocolError):
    """The server sent something the stream schema does not allow; the client ended the call."""


class SessionClosedError(SpeechError):
    """The session was used after it ended, or its sending half after it was closed."""
