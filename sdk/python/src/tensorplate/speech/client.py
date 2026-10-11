"""The streaming speech client: one channel to a stream endpoint, and the sessions on it."""

from __future__ import annotations

import asyncio
import weakref
from types import TracebackType
from typing import Any

import grpc.aio

from tensorplate.speech._session import Session
from tensorplate.speech.errors import SessionClosedError
from tensorplate.speech.events import AudioFormat, Target
from tensorplate.speech.stt import SpeechToTextSession
from tensorplate.stream.v1 import session_pb2_grpc

MAX_MESSAGE_BYTES = 64 * 1024

CHANNEL_OPTIONS = (
    # With the bandwidth probe on, gRPC widens the receive window to megabytes, and a session
    # that stops reading would not stop the server until that much was sent.
    ("grpc.http2.bdp_probe", 0),
    ("grpc.max_send_message_length", MAX_MESSAGE_BYTES),
    ("grpc.max_receive_message_length", MAX_MESSAGE_BYTES),
)


class SpeechClient:
    """Streaming speech sessions against one serving worker's stream endpoint.

    Give it one of two things. `endpoint` is the ``host:port`` of the
    worker's stream endpoint, on loopback or forwarded to it: the client
    opens a cleartext channel to it and closes that channel with itself.
    `channel` is a ``grpc.aio`` channel the caller made and keeps: the
    client never closes it, and its options are the caller's, among them
    ``grpc.http2.bdp_probe`` 0, without which a session that stops reading
    does not hold the server back.

    Create the client inside a running event loop and close it with
    ``aclose()`` or ``async with``. The client is asyncio only. It never
    retries: a session that was refused, failed or lost is reported once,
    and opening another is the caller's decision.
    """

    def __init__(self, endpoint: str | None = None, *, channel: object | None = None) -> None:
        if (endpoint is None) == (channel is None):
            raise ValueError("give the client an endpoint or a channel, and not both")
        self._owns_channel = channel is None
        self._channel: Any = channel
        if channel is None:
            self._channel = grpc.aio.insecure_channel(endpoint, options=CHANNEL_OPTIONS)
        self._stub = session_pb2_grpc.SessionServiceStub(self._channel)
        self._sessions: weakref.WeakSet[Session[Any]] = weakref.WeakSet()
        self._closed = False

    def transcribe(
        self,
        target: Target,
        *,
        language: str,
        input_format: AudioFormat,
        trace_id: str = "",
        turn_id: str = "",
        open_timeout: float | None = 10.0,
        close_timeout: float | None = 30.0,
        cancel_timeout: float = 5.0,
    ) -> SpeechToTextSession:
        """A speech-to-text session on `target`, not yet open: enter it with ``async with``.

        `language` and `input_format` must be ones the deployment declares.
        The three timeouts are in seconds. `open_timeout` bounds the wait
        for the server's answer to the open. `close_timeout` bounds
        ``close()`` and a clean exit from ``async with``; past it the call
        is cancelled. `cancel_timeout` is how long ``cancel()``, and an exit
        on an exception or a cancellation, wait for the server to end the
        session before the call is cancelled. ``None`` leaves the open or
        the close unbounded. Raises ``SessionClosedError`` on a closed
        client.
        """
        if self._closed:
            raise SessionClosedError("the client is closed")
        session = SpeechToTextSession._create(
            self._stub.Session,
            target,
            language=language,
            input_format=input_format,
            trace_id=trace_id,
            turn_id=turn_id,
            open_timeout=open_timeout,
            close_timeout=close_timeout,
            cancel_timeout=cancel_timeout,
        )
        self._sessions.add(session)
        return session

    async def aclose(self) -> None:
        """End every session made from this client, then close the channel the client opened.

        A session still open has its call cancelled and its unread events
        dropped, and from then on raises ``SessionClosedError``; so does
        opening one that was not open yet. The client waits for each to end
        for no longer than that session's `cancel_timeout`. A channel the
        caller passed in stays open.
        """
        self._closed = True
        waits = []
        bound = 0.0
        for session in list(self._sessions):
            if session._ended:
                continue
            session._shut()
            waits.append(asyncio.ensure_future(session._until_ended()))
            bound = max(bound, session._cancel_timeout)
        if waits:
            _, late = await asyncio.wait(waits, timeout=bound)
            for wait in late:
                wait.cancel()
        if self._owns_channel:
            await self._channel.close()

    async def __aenter__(self) -> SpeechClient:
        return self

    async def __aexit__(
        self,
        kind: type[BaseException] | None,
        error: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        await self.aclose()
