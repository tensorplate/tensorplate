"""The streaming speech client: one channel to a stream endpoint, and the sessions on it."""

from __future__ import annotations

import asyncio
import weakref
from collections.abc import Callable
from types import TracebackType
from typing import Any

import grpc.aio

from tensorplate.speech._session import Session
from tensorplate.speech.discovery import (
    DEFAULT_STATUS_TIMEOUT,
    AgentTargets,
    resolve_stream_target,
)
from tensorplate.speech.errors import SessionClosedError
from tensorplate.speech.events import AudioFormat, Target
from tensorplate.speech.stt import SpeechToTextSession
from tensorplate.speech.tts import Follow, TextToSpeechSession
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
    does not hold the server back. Such a client opens each session on the
    `target` it is given. ``from_agent()`` makes one that asks the agent
    instead, and follows the active deployment from one generation to the
    next.

    Create the client inside a running event loop and close it with
    ``aclose()`` or ``async with``. The client is asyncio only. It retries
    nothing but an Open whose call ended before any server event, once (see
    ``open()``): a session that was refused, failed or lost is reported
    once, and opening another is the caller's decision.
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
        self._targets: AgentTargets | None = None
        # From the agent's status, a channel per endpoint it named. One the deployment left
        # stays until the client closes: a channel with no call opens no connection.
        self._stubs: dict[str, Any] = {}
        self._channels: list[Any] = []

    @classmethod
    async def from_agent(
        cls,
        *,
        profile: str | None = None,
        config_path: str | None = None,
        status_timeout: float = DEFAULT_STATUS_TIMEOUT,
    ) -> SpeechClient:
        """A client whose sessions open on the active deployment, as the agent's status names it.

        The status is read now, through the CLI profile as
        ``resolve_stream_target()`` reads it, and raises what that raises.
        Each session then opens on the endpoint and target last read, with no
        `target` given. The status is read again before an open after a
        session ended because its worker went away (``worker_shutdown``,
        ``deployment_retired``), and once when an open's first call ends
        before any server event: when the agent then names another target
        the open's second call goes there, in place of the one more call
        that ``open()`` otherwise makes, so an open never makes more than two
        calls. A session already open is never moved to another generation.
        `status_timeout` bounds each socket operation of a read, in seconds;
        a read during an open counts toward its `open_timeout`.
        """
        first = await resolve_stream_target(
            profile=profile, config_path=config_path, timeout=status_timeout
        )
        client = cls(first.endpoint)
        client._stubs[first.endpoint] = client._stub
        client._channels.append(client._channel)
        client._targets = AgentTargets(
            first,
            client._connector,
            profile=profile,
            config_path=config_path,
            timeout=status_timeout,
        )
        return client

    def _connector(self, endpoint: str) -> Callable[[], Any]:
        if endpoint not in self._stubs:
            channel = grpc.aio.insecure_channel(endpoint, options=CHANNEL_OPTIONS)
            self._channels.append(channel)
            self._stubs[endpoint] = session_pb2_grpc.SessionServiceStub(channel)
        connect: Callable[[], Any] = self._stubs[endpoint].Session
        return connect

    def _addressed(self, target: Target | None) -> tuple[Any, Target]:
        if self._targets is None:
            if target is None:
                raise ValueError("give the session a target, or make the client with from_agent()")
            return self._stub.Session, target
        if target is not None:
            raise ValueError("a client made by from_agent() opens on the target the agent names")
        current = self._targets.current
        return self._connector(current.endpoint), current.target

    def transcribe(
        self,
        target: Target | None = None,
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

        `target` is required unless the client was made by ``from_agent()``,
        and refused if it was. `language` and `input_format` must be ones
        the deployment declares.
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
        connect, addressed = self._addressed(target)
        session = SpeechToTextSession._create(
            connect,
            addressed,
            language=language,
            input_format=input_format,
            trace_id=trace_id,
            turn_id=turn_id,
            open_timeout=open_timeout,
            close_timeout=close_timeout,
            cancel_timeout=cancel_timeout,
            targets=self._targets,
        )
        self._sessions.add(session)
        return session

    def synthesize(
        self,
        target: Target | None = None,
        *,
        language: str,
        voice: str,
        trace_id: str = "",
        turn_id: str = "",
        open_timeout: float | None = 10.0,
        close_timeout: float | Follow | None = Follow.FINALIZE_DEADLINE,
        cancel_timeout: float = 5.0,
    ) -> TextToSpeechSession:
        """A text-to-speech session on `target`, not yet open: enter it with ``async with``.

        `target` is as for ``transcribe()``. `language` and `voice` must be
        ones the deployment declares. The
        three timeouts are those of ``transcribe()``, and a `close_timeout`
        in seconds, or ``None``, means what it means there. Its default,
        ``Follow.FINALIZE_DEADLINE``, follows the server: a close ends once
        the server has sent the audio of every accepted segment, which it
        does no faster than the caller takes that audio, and the server
        allows itself ``limits.finalize_deadline_ms`` for it, as the
        session's ``Ready`` declares. The bound is that deadline and 30
        seconds more: 150 seconds for a deadline of two minutes. Raises
        ``SessionClosedError`` on a closed client.
        """
        if self._closed:
            raise SessionClosedError("the client is closed")
        connect, addressed = self._addressed(target)
        session = TextToSpeechSession._create(
            connect,
            addressed,
            language=language,
            voice=voice,
            trace_id=trace_id,
            turn_id=turn_id,
            open_timeout=open_timeout,
            close_timeout=close_timeout,
            cancel_timeout=cancel_timeout,
            targets=self._targets,
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
            for channel in self._channels or [self._channel]:
                await channel.close()

    async def __aenter__(self) -> SpeechClient:
        return self

    async def __aexit__(
        self,
        kind: type[BaseException] | None,
        error: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        await self.aclose()
