"""The streaming speech client: one channel to a stream endpoint, and the sessions on it."""

from __future__ import annotations

from types import TracebackType

import grpc.aio

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

    `endpoint` is the ``host:port`` of the worker's stream endpoint, on
    loopback or forwarded to it; the channel is cleartext. Create the client
    inside a running event loop and close it with ``aclose()`` or ``async
    with``. The client is asyncio only. It never retries: a session that was
    refused, failed or lost is reported once, and opening another is the
    caller's decision.
    """

    def __init__(self, endpoint: str) -> None:
        self._channel = grpc.aio.insecure_channel(endpoint, options=CHANNEL_OPTIONS)
        self._stub = session_pb2_grpc.SessionServiceStub(self._channel)

    def transcribe(
        self,
        target: Target,
        *,
        language: str,
        input_format: AudioFormat,
        trace_id: str = "",
        turn_id: str = "",
        open_timeout: float | None = 10.0,
    ) -> SpeechToTextSession:
        """A speech-to-text session on `target`, not yet open: enter it with ``async with``.

        `language` and `input_format` must be ones the deployment declares.
        `open_timeout`, in seconds, bounds the wait for the server's answer
        to the open.
        """
        return SpeechToTextSession(
            self._stub,
            target,
            language=language,
            input_format=input_format,
            trace_id=trace_id,
            turn_id=turn_id,
            open_timeout=open_timeout,
        )

    async def aclose(self) -> None:
        """Close the channel; sessions still open on it end."""
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
