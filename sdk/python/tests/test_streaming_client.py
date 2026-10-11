"""The streaming speech client, against the in-process server of ``stream_server.py``.

Every session here runs over a loopback gRPC channel. The server refuses
what the schema does not allow, so a client that sends it fails these tests
through the server's answer.
"""

from __future__ import annotations

import asyncio
import dataclasses
import os
from collections.abc import Awaitable, Callable
from typing import Any

import pytest

from tensorplate.errors import ErrorCode, MissingDependencyError, ProtocolError, TensorPlateError

try:
    import tensorplate.speech as speech
except MissingDependencyError as missing:
    if os.environ.get("TENSORPLATE_REQUIRE_SPEECH_EXTRAS") == "1":
        raise
    pytest.skip(str(missing), allow_module_level=True)

import grpc.aio

from stream_server import StreamServer, recorded
from tensorplate.speech import _session, client
from tensorplate.speech._buffer import EventBuffer
from tensorplate.stream.v1 import session_pb2 as pb

TARGET = speech.Target("synthetic-deployment", 7, "sha256:" + "0" * 64)
PCM = speech.AudioFormat("pcm_s16le", 16000)
# 20 ms; the server grants one second, which is fifty of them.
FRAME = bytes(640)
Scenario = Callable[[StreamServer, speech.SpeechToTextSession], Awaitable[Any]]


def play(scenario: Scenario, server: StreamServer | None = None, **options: Any) -> Any:
    """Run `scenario` with a served `server` and a session on it that is not yet open."""
    fake = server or StreamServer()
    options = {"language": "en", "input_format": PCM, **options}
    target = options.pop("target", TARGET)

    async def main() -> Any:
        async with fake.serving() as endpoint, speech.SpeechClient(endpoint) as stream:
            return await scenario(fake, stream.transcribe(target, **options))

    return asyncio.run(asyncio.wait_for(main(), 60))


async def taken(session: speech.SpeechToTextSession) -> list[Any]:
    return [event async for event in session]


async def settle() -> None:
    """Let what is already on its way arrive."""
    await asyncio.sleep(0.2)


def test_audio_within_the_credit_then_finalize_returns_the_one_final_transcript() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            assert session.session_id == "synthetic-session-1"
            assert session.limits.speech_to_text == speech.SpeechToTextLimits(20, 40, 30000)
            assert session.limits.heartbeat_interval_ms == 10000
            assert session.remaining_duration_ms == 3600000
            for _ in range(5):
                await session.send_audio(FRAME)
            final = await session.finalize()
            totals = await session.close()
        segment = speech.TranscriptSegment(0, 100000, "synthetic transcript")
        assert final == speech.FinalTranscript(1, (segment,), 1600, finalize_sequence=7)
        assert final.text == "synthetic transcript"
        assert await taken(session) == [speech.EndpointDetected(1, 1600, "client_finalize"), final]
        assert totals == speech.SessionTotals(1600, 0, 0, 1, 0)
        assert fake.bodies() == ["open", *["audio"] * 5, "finalize"]
        wire = pb.AudioFormat(
            encoding=pb.AUDIO_ENCODING_PCM_S16LE, sample_rate_hz=16000, channels=1
        )
        assert fake.received[0].open == pb.Open(
            resolved=recorded("stt_open").open.resolved,
            mode=pb.SESSION_MODE_STT_STREAMING,
            language="en",
            input_format=wire,
        )
        assert session.unknown_events == 0

    play(scenario)


def test_a_sender_waits_for_credit_and_resumes_when_the_server_returns_it() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async def send() -> None:
            for _ in range(60):
                await session.send_audio(FRAME)

        async with session:
            sender = asyncio.create_task(send())
            while fake.bodies().count("audio") < 50:
                await asyncio.sleep(0)
            await settle()
            assert fake.bodies().count("audio") == 50
            assert not sender.done()
            fake.consume()
            await sender
            assert (await session.finalize()).end_sample_offset == 60 * 320

    play(scenario, StreamServer(hold_credit=True))


def test_a_frame_outside_the_bounds_is_refused_before_it_is_sent() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            for data in (b"", bytes(641), bytes(642 * 2)):
                with pytest.raises(ValueError, match=r"samples|at most 40 ms"):
                    await session.send_audio(data)
            await session.send_audio(bytes(320))
            with pytest.raises(ValueError, match="shorter than 20 ms"):
                await session.send_audio(FRAME)
            await session.finalize()
            await session.send_audio(FRAME)
            assert (await session.close()).input_audio_samples == 480
        assert fake.bodies() == ["open", "audio", "finalize", "audio"]

    play(scenario)


def test_closing_ends_the_open_utterance_and_iteration_ends_with_the_session() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            await session.send_audio(FRAME)
            await session.send_audio(FRAME)
        segment = speech.TranscriptSegment(0, 40000, "synthetic transcript")
        events = await taken(session)
        assert events == [
            speech.PartialTranscript(1, 1, "synthetic"),
            speech.EndpointDetected(1, 640, "half_close"),
            speech.FinalTranscript(1, (segment,), 640, finalize_sequence=0),
        ]
        assert all(dataclasses.is_dataclass(event) for event in events)
        with pytest.raises(speech.SessionClosedError):
            await session.send_audio(FRAME)

    play(scenario, StreamServer(partials=True))


def test_a_server_event_of_an_unknown_kind_is_passed_over_and_counted() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            await session.send_audio(FRAME)
            await session.finalize()
            assert session.unknown_events == 1

    play(scenario, StreamServer(fault="unknown_event"))


@pytest.mark.parametrize(
    ("server", "options", "error", "status"),
    [
        (StreamServer(), {"language": "xx"}, speech.PreconditionFailedError, "FAILED_PRECONDITION"),
        (
            StreamServer(),
            {"target": dataclasses.replace(TARGET, generation=6)},
            speech.PreconditionFailedError,
            "FAILED_PRECONDITION",
        ),
        (
            StreamServer(),
            {"target": speech.Target("bad id", 7, TARGET.descriptor_digest)},
            speech.InvalidEventError,
            "INVALID_ARGUMENT",
        ),
        (StreamServer(admission_open=False), {}, speech.ServiceUnavailableError, "UNAVAILABLE"),
        (StreamServer(max_sessions=0), {}, speech.ResourceExhaustedError, "RESOURCE_EXHAUSTED"),
    ],
)
def test_a_refused_open_raises_the_class_of_its_status_once_and_admission_is_unknown(
    server: StreamServer, options: dict[str, Any], error: type[speech.SpeechError], status: str
) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        with pytest.raises(speech.SpeechError) as caught:
            await session.open()
        failure = caught.value
        assert type(failure) is error
        assert isinstance(failure, TensorPlateError)
        assert (failure.grpc_status, failure.reason, failure.code) == (status, None, None)
        assert failure.admitted is None
        with pytest.raises(error):
            await taken(session)
        assert fake.calls == 1

    play(scenario, server, **options)


def test_a_decoded_refusal_gives_its_reason_and_states_that_nothing_was_admitted(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    refusal = recorded("refused_session_count_limit")
    monkeypatch.setattr(_session, "decode_refusal", lambda trailing_metadata: refusal)

    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        with pytest.raises(speech.ResourceExhaustedError) as caught:
            await session.open()
        failure = caught.value
        assert (failure.reason, failure.code) == (
            "session_count_limit",
            ErrorCode.RESOURCE_EXHAUSTED,
        )
        assert failure.admitted is False

    play(scenario, StreamServer(max_sessions=0))


@pytest.mark.parametrize(
    ("reason", "error", "code", "status"),
    [
        ("worker_shutdown", speech.ServiceUnavailableError, ErrorCode.UNAVAILABLE, "UNAVAILABLE"),
        ("backend_reset", speech.ServiceUnavailableError, ErrorCode.UNAVAILABLE, "UNAVAILABLE"),
        ("idle_timeout", speech.SessionTimeoutError, ErrorCode.TIMEOUT, "DEADLINE_EXCEEDED"),
    ],
)
def test_a_session_the_server_ends_raises_its_cause_from_every_pending_call(
    reason: str, error: type[speech.SpeechError], code: ErrorCode, status: str
) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await session.send_audio(FRAME)
        finalize = asyncio.create_task(session.finalize())
        while "finalize" not in fake.bodies():
            await asyncio.sleep(0)
        fake.end(reason)
        failures = []
        for pending in (finalize, taken(session), session.send_audio(FRAME), session.close()):
            with pytest.raises(speech.SpeechError) as caught:
                await pending
            failures.append(caught.value)
        assert all(type(failure) is error for failure in failures)
        assert (failures[0].reason, failures[0].code) == (reason, code)
        assert (failures[0].admitted, failures[0].grpc_status) == (True, status)
        assert fake.calls == 1

    play(scenario, StreamServer(fault="stall_finalize"))


def test_cancel_tells_the_server_and_drops_what_was_not_taken() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await session.send_audio(FRAME)
        while session._events.used == 0:
            await asyncio.sleep(0)
        await session.cancel()
        assert await taken(session) == []
        assert fake.bodies()[-1] == "cancel"
        assert fake.closed[-1].cause == recorded("closed_client_cancelled").session_closed.cause
        for call in (session.send_audio(FRAME), session.finalize(), session.close()):
            with pytest.raises(speech.SessionClosedError):
                await call
        await session.cancel()

    play(scenario, StreamServer(partials=True))


def test_leaving_the_session_on_an_exception_cancels_it() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        with pytest.raises(KeyError):
            async with session:
                raise KeyError("the caller's own failure")
        assert fake.bodies() == ["open", "cancel"]

    play(scenario)


def test_ping_is_sent_at_the_interval_ready_gives() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            while fake.bodies().count("ping") < 3:
                await asyncio.sleep(0.005)
            await session.send_audio(FRAME)
            await session.finalize()

    play(scenario, StreamServer(heartbeat_interval_ms=10))


def test_an_open_that_is_not_answered_in_time_ends_with_admission_unknown() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        with pytest.raises(speech.SessionTimeoutError) as caught:
            await session.open()
        assert (caught.value.admitted, caught.value.grpc_status) == (None, None)

    play(scenario, StreamServer(fault="silent"), open_timeout=0.05)


def test_a_finalize_that_is_not_answered_in_time_ends_the_session() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await session.send_audio(FRAME)
        with pytest.raises(speech.SessionTimeoutError) as caught:
            await session.finalize(timeout=0.05)
        assert caught.value.admitted is True
        with pytest.raises(speech.SessionTimeoutError):
            await session.send_audio(FRAME)

    play(scenario, StreamServer(fault="stall_finalize"))


@pytest.mark.parametrize(
    "fault",
    [
        "sequence_gap",
        "event_before_ready",
        "other_generation",
        "no_credit",
        "final_without_endpoint",
        "partial_after_endpoint",
        "other_finalize",
        "ok_with_cause",
        "ok_without_closed",
    ],
)
def test_a_server_that_breaks_the_protocol_ends_the_session_with_a_protocol_error(
    fault: str,
) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async def use() -> None:
            async with session:
                await session.send_audio(FRAME)
                if fault == "ok_with_cause":
                    fake.end("worker_shutdown")
                await session.finalize()

        with pytest.raises(speech.StreamProtocolError) as caught:
            await use()
        assert isinstance(caught.value, ProtocolError)
        assert caught.value.grpc_status is None

    play(scenario, StreamServer(fault=fault))


def test_a_call_lost_after_ready_raises_the_class_of_its_status_as_admitted() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        with pytest.raises(speech.ServiceUnavailableError) as caught:
            await session.close()
        assert (caught.value.admitted, caught.value.reason) == (True, None)

    play(scenario, StreamServer(fault="drop"))


def test_an_endpoint_that_cannot_be_reached_is_unavailable() -> None:
    async def main() -> None:
        async with speech.SpeechClient("127.0.0.1:1") as stream:
            session = stream.transcribe(TARGET, language="en", input_format=PCM)
            with pytest.raises(speech.ServiceUnavailableError) as caught:
                await session.open()
            assert caught.value.admitted is None

    asyncio.run(asyncio.wait_for(main(), 60))


def test_two_tasks_sending_at_once_keep_one_order_on_the_wire() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async def send() -> None:
            for _ in range(20):
                await session.send_audio(FRAME)

        def ping() -> Any:
            return session._send(pb.ClientEvent(ping=pb.Ping()))

        async with session:
            await asyncio.gather(send(), send(), *(ping() for _ in range(20)))
            assert (await session.finalize()).end_sample_offset == 40 * 320
        assert fake.bodies().count("ping") == 20

    play(scenario)


def test_cancel_ends_a_session_whose_writer_the_transport_holds_back() -> None:
    stereo = speech.AudioFormat("pcm_s16le", 48000, channels=2)

    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async def send() -> None:
            for _ in range(3):
                await session.send_audio(bytes(320 * 192))

        await session.open()
        sender = asyncio.create_task(send())
        await settle()
        assert session._write_lock.locked()
        assert not sender.done()
        await session.cancel(timeout=0.1)
        with pytest.raises(speech.SessionClosedError):
            await sender

    server = StreamServer(
        fault="deaf", formats=((pb.AUDIO_ENCODING_PCM_S16LE, 48000, 2),), max_frame_ms=320
    )
    play(scenario, server, input_format=stereo)


def test_the_channel_keeps_the_http2_window_fixed_and_bounds_message_size(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    built: list[dict[str, Any]] = []
    channel = grpc.aio.insecure_channel

    def build(endpoint: str, options: Any = None) -> Any:
        built.append(dict(options))
        return channel(endpoint, options=options)

    monkeypatch.setattr(grpc.aio, "insecure_channel", build)

    async def main() -> None:
        async with speech.SpeechClient("127.0.0.1:1"):
            pass

    asyncio.run(main())
    assert built == [
        {
            "grpc.http2.bdp_probe": 0,
            "grpc.max_send_message_length": client.MAX_MESSAGE_BYTES,
            "grpc.max_receive_message_length": client.MAX_MESSAGE_BYTES,
        }
    ]
    assert client.MAX_MESSAGE_BYTES == 64 * 1024


def test_the_event_buffer_holds_back_a_put_while_it_is_full() -> None:
    async def main() -> None:
        buffer: EventBuffer[str] = EventBuffer(10)
        await buffer.put("large", 25)
        blocked = asyncio.create_task(buffer.put("next", 4))
        await settle()
        assert not blocked.done()
        assert buffer.used == 25
        assert await buffer.get() == "large"
        await blocked
        await buffer.put("more", 6)
        assert buffer.used == 10
        late = asyncio.create_task(buffer.put("late", 1))
        buffer.end()
        await late
        assert [await buffer.get(), await buffer.get(), await buffer.get()] == [
            "next",
            "more",
            None,
        ]
        buffer.end(discard=True)
        assert buffer.used == 0

    asyncio.run(asyncio.wait_for(main(), 60))


def test_the_public_names_are_the_sdks_own_types() -> None:
    for name in speech.__all__:
        exported = getattr(speech, name)
        if isinstance(exported, type):
            assert exported.__module__.startswith("tensorplate.speech."), name
            assert not exported.__module__.startswith("tensorplate.stream"), name
    assert issubclass(speech.SpeechError, TensorPlateError)
