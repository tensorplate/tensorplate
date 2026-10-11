"""The streaming speech client, against the in-process server of ``stream_server.py``.

Every session here runs over a loopback gRPC channel. The server refuses
what the schema does not allow, so a client that sends it fails these tests
through the server's answer.
"""

from __future__ import annotations

import array
import asyncio
import dataclasses
import gc
import inspect
import os
import typing
from collections.abc import Awaitable, Callable, Coroutine, Iterator
from typing import Any

import pytest

from tensorplate.errors import (
    ErrorCode,
    MissingDependencyError,
    ProtocolError,
    RequestTimeoutError,
    TensorPlateError,
    TransportError,
)

try:
    import tensorplate.speech as speech
except MissingDependencyError as missing:
    if os.environ.get("TENSORPLATE_REQUIRE_SPEECH_EXTRAS") == "1":
        raise
    pytest.skip(str(missing), allow_module_level=True)

import grpc
import grpc.aio

from stream_server import REPOSITORY, Playback, StreamServer, frames, recorded
from tensorplate.speech import _session, client
from tensorplate.speech._buffer import EventBuffer
from tensorplate.stream.v1 import session_pb2 as pb

TARGET = speech.Target("synthetic-deployment", 7, "sha256:" + "0" * 64)
PCM = speech.AudioFormat("pcm_s16le", 16000)
MULAW = speech.AudioFormat("mulaw", 8000)
STEREO = speech.AudioFormat("pcm_s16le", 48000, channels=2)
# 20 ms; the server grants one second, which is fifty of them.
FRAME = bytes(640)
# 320 ms of STEREO: one fits the 64 KiB HTTP/2 window and a second does not.
LARGE = bytes(320 * 192)
Scenario = Callable[[StreamServer, speech.SpeechToTextSession], Awaitable[Any]]


def run(main: Callable[[], Awaitable[Any]]) -> Any:
    """Run `main` in a loop of its own; nothing may be left for asyncio to report."""
    reported: list[dict[str, Any]] = []

    async def watched() -> Any:
        asyncio.get_running_loop().set_exception_handler(
            lambda _, context: reported.append(context)
        )
        result = await main()
        # A task that died with an exception nobody took is reported when it is collected.
        gc.collect()
        await asyncio.sleep(0)
        return result

    result = asyncio.run(asyncio.wait_for(watched(), 60))
    assert reported == []
    return result


def play(scenario: Scenario, server: StreamServer | None = None, **options: Any) -> Any:
    """Run `scenario` with a served `server` and a session on it that is not yet open."""
    fake = server or StreamServer()
    options = {"language": "en", "input_format": PCM, **options}
    target = options.pop("target", TARGET)

    async def main() -> Any:
        async with fake.serving() as endpoint, speech.SpeechClient(endpoint) as stream:
            return await scenario(fake, stream.transcribe(target, **options))

    return run(main)


def deaf() -> StreamServer:
    """A server that reads nothing after the Open until `hear()`, serving STEREO."""
    return StreamServer(
        fault="deaf", formats=((pb.AUDIO_ENCODING_PCM_S16LE, 48000, 2),), max_frame_ms=320
    )


async def taken(session: speech.SpeechToTextSession) -> list[Any]:
    return [event async for event in session]


async def collect(session: speech.SpeechToTextSession, events: list[Any]) -> None:
    """Take the session's events into `events`, which keeps them when the session fails."""
    async for event in session:
        events.append(event)


async def send(session: speech.SpeechToTextSession, count: int, frame: bytes) -> None:
    for _ in range(count):
        await session.send_audio(frame)


async def turns() -> None:
    """Let every task that can run without the server run as far as it can."""
    for _ in range(10):
        await asyncio.sleep(0)


async def barrier(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
    """Return once the server has taken everything the session wrote before this call."""
    pings = fake.bodies().count("ping")
    await session._send(pb.ClientEvent(ping=pb.Ping()))
    await fake.until(lambda: fake.bodies().count("ping") > pings)


async def held_writer(session: speech.SpeechToTextSession) -> asyncio.Task[None]:
    """Start sending LARGE frames to a deaf server; return once the transport holds the second."""

    sender = asyncio.create_task(send(session, 3, LARGE))
    while session._sent < 3 or session._writer is None:
        await asyncio.sleep(0)
    return sender


async def flood(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
    """Have the server send 20 KiB of partial transcripts; return once the session reads no more."""
    for _ in range(20):
        fake.partial("x" * 1000)
    # Sixteen fit; the reader holds the seventeenth, which is its eighteenth event.
    while session._received < 18:
        await asyncio.sleep(0)


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
        async with session:
            sender = asyncio.create_task(send(session, 60, FRAME))
            await fake.until(lambda: fake.bodies().count("audio") == 50)
            await turns()
            await barrier(fake, session)
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


@pytest.mark.parametrize("fault", ["unknown_event", "audio_chunk"])
def test_a_server_event_this_session_does_not_know_is_passed_over_and_counted(fault: str) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            await session.send_audio(FRAME)
            await session.finalize()
            assert session.unknown_events == 1
        assert [type(event) for event in await taken(session)] == [
            speech.EndpointDetected,
            speech.FinalTranscript,
        ]

    play(scenario, StreamServer(fault=fault))


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
        assert type(failure.grpc_status) is str
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
        await fake.until(lambda: "finalize" in fake.bodies())
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

    play(scenario, StreamServer(hold_finals=True))


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

    play(scenario, StreamServer(hold_finals=True))


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
        "second_endpoint",
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

    run(main)


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
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        sender = await held_writer(session)
        assert session._write_lock.locked()
        await session.cancel(timeout=0.1)
        with pytest.raises(speech.SessionClosedError):
            await sender

    play(scenario, deaf(), input_format=STEREO)


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
        await turns()
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

    run(main)


def test_the_event_buffer_lets_a_held_put_through_once_unbound() -> None:
    async def main() -> None:
        buffer: EventBuffer[str] = EventBuffer(10)
        await buffer.put("first", 10)
        with pytest.raises(asyncio.TimeoutError):
            await buffer.put("refused", 1, 0.01)
        held = asyncio.create_task(buffer.put("held", 1, 30))
        await turns()
        assert not held.done()
        buffer.unbound()
        await held
        await buffer.put("more", 100)
        assert buffer.used == 111

    run(main)


def test_the_recorded_session_is_what_the_client_sends_and_what_it_reads() -> None:
    session_frames = frames("stt_session")
    sent = [(name, encoded) for name, kind, encoded in session_frames if kind == "ClientEvent"]
    audio = [recorded(name).audio.data for name, _ in sent if name.startswith("stt_audio")]
    opening = recorded("stt_open").open
    playback = Playback("stt_session")

    async def main() -> tuple[Any, ...]:
        async with playback.serving() as endpoint, speech.SpeechClient(endpoint) as stream:
            session = stream.transcribe(
                TARGET,
                language=opening.language,
                input_format=MULAW,
                trace_id=opening.trace_id,
                turn_id=opening.turn_id,
            )
            async with session:
                reader = asyncio.create_task(taken(session))
                for data in audio[:5]:
                    await session.send_audio(data)
                second = await session.finalize()
                await session.send_audio(audio[5])
                third = await session.finalize()
                await session.send_audio(audio[6])
                await session.send_audio(audio[7])
                totals = await session.close()
            return await reader, second, third, totals

    events, second, third, totals = run(main)

    assert playback.received == [recorded(name) for name, _ in sent]
    assert [event.SerializeToString() for event in playback.received] == [e for _, e in sent]
    part = speech.TranscriptSegment
    finals = [
        speech.FinalTranscript(
            1, (part(0, 30000, "synthetic"), part(30000, 60000, " transcript")), 480, 0
        ),
        speech.FinalTranscript(
            2, (part(75000, 100000, "synthetic"), part(100000, 110000, " words")), 880, 7
        ),
        speech.FinalTranscript(3, (), 1040, 9),
        speech.FinalTranscript(4, (part(135000, 160000, "synthetic end"),), 1280, 0),
    ]
    assert events == [
        speech.PartialTranscript(1, 1, "synthetic"),
        speech.PartialTranscript(1, 2, "synthetic transcript"),
        speech.EndpointDetected(1, 480, "silence"),
        finals[0],
        speech.PartialTranscript(2, 1, "synthetic words"),
        speech.EndpointDetected(2, 880, "client_finalize"),
        finals[1],
        speech.EndpointDetected(3, 1040, "client_finalize"),
        finals[2],
        speech.PartialTranscript(4, 1, "synthetic end"),
        speech.EndpointDetected(4, 1280, "half_close"),
        finals[3],
    ]
    assert (second, third) == (finals[1], finals[2])
    assert [final.text for final in finals[1:3]] == ["synthetic words", ""]
    assert totals == speech.SessionTotals(1280, 0, 0, 4, 0)


@pytest.mark.parametrize(
    ("steps", "order"),
    [
        (("final", "endpoint", "final"), [1, 1, 2, 2]),
        # The next utterance's endpoint may precede this one's final transcript.
        (("endpoint", "final", "final"), [1, 2, 1, 2]),
    ],
)
def test_a_final_transcript_that_names_no_finalize_answers_none(
    steps: tuple[str, ...], order: list[int]
) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            reader = asyncio.create_task(taken(session))
            await session.send_audio(FRAME)
            await fake.until(lambda: "audio" in fake.bodies())
            fake.endpoint()
            finalizing = asyncio.create_task(session.finalize())
            await fake.until(lambda: "finalize" in fake.bodies())
            for step in steps:
                getattr(fake, step)()
            answer = await finalizing
        events = await reader
        assert answer == speech.FinalTranscript(2, (), 320, finalize_sequence=3)
        assert [event.utterance_id for event in events] == order
        assert [event.reason for event in events if isinstance(event, speech.EndpointDetected)] == [
            "silence",
            "client_finalize",
        ]
        finals = [event for event in events if isinstance(event, speech.FinalTranscript)]
        assert [final.finalize_sequence for final in finals] == [0, 3]

    play(scenario, StreamServer(hold_finals=True))


def test_a_cancelled_finalize_holds_audio_back_until_its_answer() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            await session.send_audio(FRAME)
            finalizing = asyncio.create_task(session.finalize())
            await fake.until(lambda: "finalize" in fake.bodies())
            finalizing.cancel()
            await asyncio.wait({finalizing})
            assert finalizing.cancelled()
            sender = asyncio.create_task(session.send_audio(FRAME))
            await turns()
            await barrier(fake, session)
            assert fake.bodies() == ["open", "audio", "finalize", "ping"]
            assert not sender.done()
            fake.final()
            await sender
            closing = asyncio.create_task(session.close())
            await fake.until(lambda: fake.half_closed)
            fake.final()
            totals = await closing
        # The server refuses audio while a Finalize is unanswered, so it took none before.
        assert fake.bodies() == ["open", "audio", "finalize", "ping", "audio"]
        assert totals == speech.SessionTotals(640, 0, 0, 2, 0)

    play(scenario, StreamServer(hold_finals=True))


def test_a_finalize_waits_for_the_audio_that_is_ahead_of_it() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            sender = asyncio.create_task(send(session, 51, FRAME))
            await fake.until(lambda: fake.bodies().count("audio") == 50)
            finalizing = asyncio.create_task(session.finalize())
            await turns()
            await barrier(fake, session)
            assert "finalize" not in fake.bodies()
            fake.consume()
            await sender
            await fake.until(lambda: "finalize" in fake.bodies())
            fake.final()
            assert (await finalizing).end_sample_offset == 51 * 320
        assert fake.bodies() == ["open", *["audio"] * 50, "ping", "audio", "finalize"]

    play(scenario, StreamServer(hold_credit=True, hold_finals=True))


def test_a_sender_cancelled_while_it_waits_for_credit_sends_nothing() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            sender = asyncio.create_task(send(session, 51, FRAME))
            await fake.until(lambda: fake.bodies().count("audio") == 50)
            await turns()
            sender.cancel()
            await asyncio.wait({sender})
            assert sender.cancelled()
            fake.consume()
            await session.send_audio(FRAME)
            assert (await session.finalize()).end_sample_offset == 51 * 320
        assert fake.bodies().count("audio") == 51

    play(scenario, StreamServer(hold_credit=True))


def test_a_second_finalize_waits_for_the_answer_to_the_first() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            await session.send_audio(FRAME)
            first = asyncio.create_task(session.finalize())
            await fake.until(lambda: "finalize" in fake.bodies())
            first.cancel()
            await asyncio.wait({first})
            second = asyncio.create_task(session.finalize(timeout=30))
            await turns()
            await barrier(fake, session)
            assert fake.bodies().count("finalize") == 1
            fake.final()
            await fake.until(lambda: fake.bodies().count("finalize") == 2)
            fake.final()
            answer = await second
        assert (answer.utterance_id, answer.finalize_sequence) == (2, 5)

    play(scenario, StreamServer(hold_finals=True))


def test_close_stops_the_ping_task_before_the_session_ends() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await session.send_audio(FRAME)
        closing = asyncio.create_task(session.close())
        await fake.until(lambda: fake.half_closed)
        assert session._pinger is not None
        assert session._pinger.done()
        fake.final()
        assert (await closing).utterances_completed == 1

    play(scenario, StreamServer(hold_finals=True))


def test_a_ready_without_a_heartbeat_interval_means_no_ping() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            assert session.limits.heartbeat_interval_ms == 0
            assert session._pinger is None
            await session.send_audio(FRAME)
            await session.finalize()
        assert "ping" not in fake.bodies()

    play(scenario, StreamServer(heartbeat_interval_ms=0))


USES: dict[str, Callable[[speech.SpeechToTextSession], Coroutine[Any, Any, Any]]] = {
    "iteration": taken,
    "close": lambda session: session.close(),
    "finalize": lambda session: session.finalize(timeout=30),
    "send_audio": lambda session: session.send_audio(FRAME),
}


@pytest.mark.parametrize("use", [*USES, "exit"])
def test_a_frame_that_does_not_decode_ends_the_session_for_every_use(use: str) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        if use == "exit":
            with pytest.raises(speech.StreamProtocolError):
                async with session:
                    await session._until_ended()
            return
        await session.open()
        await session._until_ended()
        with pytest.raises(speech.StreamProtocolError, match="not a server event") as caught:
            await USES[use](session)
        assert (caught.value.admitted, caught.value.grpc_status) == (True, None)
        assert caught.value.__cause__ is None
        await session.cancel()

    play(scenario, StreamServer(fault="undecodable"))


@pytest.mark.parametrize("use", ["iteration", "close", "finalize", "cancel"])
def test_a_use_that_waits_is_woken_by_a_frame_that_does_not_decode(use: str) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        if use == "cancel":
            waiting = asyncio.create_task(session.cancel())
            await fake.until(lambda: fake.live == 1)
            await turns()
            fake.hear()
            await waiting
            assert await taken(session) == []
            return
        waiting = asyncio.create_task(USES[use](session))
        await turns()
        fake.hear()
        with pytest.raises(speech.StreamProtocolError):
            await waiting

    play(scenario, StreamServer(fault="deaf_undecodable"))


def test_a_reader_that_fails_ends_the_session_with_the_failure_as_its_cause(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    failure = RuntimeError("a defect in the client")

    def fail(event: Any) -> None:
        raise failure

    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        monkeypatch.setattr(session, "_task_event", fail)
        await session.open()
        await session.send_audio(FRAME)
        for call in (session.finalize(), session.close(), taken(session)):
            with pytest.raises(speech.StreamProtocolError, match="RuntimeError") as caught:
                await call
            assert caught.value.__cause__ is failure
        await session.cancel()

    play(scenario, StreamServer(partials=True))


def test_a_reader_cancelled_while_it_is_held_still_ends_the_session() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await flood(fake, session)
        assert session._reader is not None
        session._reader.cancel()
        with pytest.raises(speech.SessionCancelledError):
            await session.close(timeout=1)
        await fake.until(lambda: fake.live == 0)

    play(scenario)


def test_the_documented_example_reads_events_while_it_sends(
    capsys: pytest.CaptureFixture[str],
) -> None:
    page = (REPOSITORY / "docs/sdk/python.md").read_text(encoding="utf-8")
    section = page[page.index("## Streaming speech client") :]
    example: dict[str, Any] = {}
    exec(section.split("```python\n", 1)[1].split("```", 1)[0], example)
    # Forty frames draw 40 KiB of partial transcripts, more than a session holds unread.
    fake = StreamServer(
        deployment_id="my-deployment", generation=0, descriptor_digest="", partial_bytes=1000
    )

    async def main() -> Any:
        async with fake.serving() as endpoint:
            return await example["transcribe"](endpoint, [FRAME] * 40)

    assert run(main) == "synthetic transcript"
    assert capsys.readouterr().out.count("x" * 1000) == 40
    assert fake.bodies() == ["open", *["audio"] * 40, "finalize"]


def test_a_session_whose_events_are_left_unread_ends_with_a_typed_error() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await flood(fake, session)
        # More than the credit, with nothing reading: the Accepted are behind the events.
        with pytest.raises(speech.ResourceExhaustedError, match="left unread") as caught:
            await send(session, 60, FRAME)
        failure = caught.value
        assert (failure.admitted, failure.grpc_status, failure.reason) == (True, None, None)
        for call in (session.finalize(), session.close()):
            with pytest.raises(speech.ResourceExhaustedError):
                await call
        events: list[Any] = []
        with pytest.raises(speech.ResourceExhaustedError):
            await collect(session, events)
        assert len(events) == 16
        await fake.until(lambda: fake.live == 0)

    play(scenario, StreamServer(output_stall_timeout_ms=50))


def test_a_sender_waiting_for_credit_raises_the_cause_the_server_ends_the_session_with() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        sender = asyncio.create_task(send(session, 60, FRAME))
        await fake.until(lambda: fake.bodies().count("audio") == 50)
        await turns()
        fake.end("slow_consumer")
        with pytest.raises(speech.ResourceExhaustedError) as caught:
            await sender
        failure = caught.value
        assert (failure.reason, failure.grpc_status) == ("slow_consumer", "RESOURCE_EXHAUSTED")
        assert (failure.code, failure.admitted) == (ErrorCode.RESOURCE_EXHAUSTED, True)
        assert fake.bodies().count("audio") == 50

    play(scenario, StreamServer(hold_credit=True))


def test_a_clean_exit_is_bounded_by_the_sessions_close_timeout_and_then_cancels_the_call() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        with pytest.raises(speech.SessionTimeoutError, match="end of the session") as caught:
            async with session:
                await session.send_audio(FRAME)
        assert caught.value.admitted is True
        assert fake.half_closed
        await fake.until(lambda: fake.live == 0)
        with pytest.raises(speech.SessionTimeoutError):
            await session.close()

    play(scenario, StreamServer(hold_finals=True), close_timeout=0.05)


def test_the_bounds_given_with_the_session_are_the_ones_its_exits_use(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        bounds: list[float | None] = []
        within = session._within

        async def spy(work: Any, timeout: float | None) -> Any:
            bounds.append(timeout)
            return await within(work, timeout)

        monkeypatch.setattr(session, "_within", spy)
        async with session:
            pass
        assert bounds == [3.0, 11.0]

    play(scenario, open_timeout=3.0, close_timeout=11.0, cancel_timeout=7.0)

    async def abandoned(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        bounds: list[float | None] = []
        within = session._within

        async def spy(work: Any, timeout: float | None) -> Any:
            bounds.append(timeout)
            return await within(work, timeout)

        monkeypatch.setattr(session, "_within", spy)
        with pytest.raises(KeyError):
            async with session:
                raise KeyError("the caller's own failure")
        assert bounds == [3.0, 7.0]

    play(abandoned, open_timeout=3.0, close_timeout=11.0, cancel_timeout=7.0)


@pytest.mark.parametrize("use", ["finalize", "close"])
def test_a_timeout_runs_from_the_call_and_covers_the_wait_behind_another_writer(use: str) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        sender = await held_writer(session)
        with pytest.raises(speech.SessionTimeoutError) as caught:
            await getattr(session, use)(timeout=0.05)
        assert caught.value.admitted is True
        with pytest.raises(speech.SessionTimeoutError):
            await sender

    play(scenario, deaf(), input_format=STEREO)


def test_a_sender_cancelled_while_the_session_is_cancelled_reads_as_cancelled() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        sender = await held_writer(session)
        cancelling = asyncio.create_task(session.cancel(timeout=0.05))
        await turns()
        sender.cancel()
        await asyncio.wait({sender})
        assert sender.cancelled()
        await cancelling
        with pytest.raises(speech.SessionClosedError):
            await session.send_audio(LARGE)

    play(scenario, deaf(), input_format=STEREO)


def test_a_sender_cancelled_in_a_write_leaves_the_session_open_and_its_frame_sent() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            sender = await held_writer(session)
            sender.cancel()
            await asyncio.wait({sender})
            assert sender.cancelled()
            fake.hear()
            await session.send_audio(LARGE)
            final = await session.finalize()
        assert final.end_sample_offset == 3 * len(LARGE) // STEREO.sample_width
        assert fake.bodies() == ["open", "audio", "audio", "audio", "finalize"]

    play(scenario, deaf(), input_format=STEREO)


def test_a_task_cancelled_in_a_clean_exit_abandons_the_session() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async def use() -> None:
            async with session:
                await session.send_audio(FRAME)

        user = asyncio.create_task(use())
        await fake.until(lambda: fake.half_closed)
        user.cancel()
        await asyncio.wait({user})
        assert user.cancelled()
        await fake.until(lambda: fake.live == 0)
        with pytest.raises(speech.SessionClosedError):
            await session.send_audio(FRAME)

    play(scenario, StreamServer(hold_finals=True))


def test_a_write_grpc_refuses_ends_the_session_with_a_typed_error_past_a_full_buffer(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def refuse(event: Any) -> None:
        raise grpc.aio.UsageError("synthetic")

    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await flood(fake, session)
        monkeypatch.setattr(session._call, "write", refuse)
        with pytest.raises(speech.SessionCancelledError) as caught:
            await session.send_audio(FRAME)
        assert caught.value.__cause__ is None
        assert caught.value.__suppress_context__
        events: list[Any] = []
        with pytest.raises(speech.SessionCancelledError):
            await collect(session, events)
        # The reader was let past the bound with the event it held.
        assert len(events) == 17

    play(scenario)


def test_send_audio_takes_any_bytes_like_object() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        samples = array.array("h", range(320))
        async with session:
            for data in (samples.tobytes(), bytearray(samples.tobytes()), memoryview(samples)):
                await session.send_audio(data)
            assert (await session.finalize()).end_sample_offset == 960
        sent = [event.audio for event in fake.received if event.HasField("audio")]
        assert [audio.data for audio in sent] == [samples.tobytes()] * 3
        assert [audio.sample_count for audio in sent] == [320] * 3

    play(scenario)


def test_closing_the_client_ends_the_sessions_made_from_it() -> None:
    async def main() -> None:
        fake = StreamServer(heartbeat_interval_ms=10)
        async with fake.serving() as endpoint:
            async with speech.SpeechClient(endpoint) as stream:
                options: dict[str, Any] = {"language": "en", "input_format": PCM}
                finished = stream.transcribe(TARGET, **options)
                async with finished:
                    await finished.send_audio(FRAME)
                session = stream.transcribe(TARGET, **options)
                unopened = stream.transcribe(TARGET, **options)
                await session.open()
                await fake.until(lambda: "ping" in fake.bodies())
                waiting = asyncio.create_task(taken(session))
            await fake.until(lambda: fake.live == 0)
            assert session._pinger is not None
            assert session._pinger.done()
            assert await waiting == []
            for closed in (session, unopened):
                calls = (closed.open(), closed.send_audio(FRAME), closed.finalize(), closed.close())
                for call in calls:
                    with pytest.raises(speech.SessionClosedError):
                        await call
                with pytest.raises(speech.SessionClosedError):
                    async with closed:
                        pass
                await closed.cancel()
                assert await taken(closed) == []
            with pytest.raises(speech.SessionClosedError):
                stream.transcribe(TARGET, **options)
            await stream.aclose()
            # A session that had ended keeps its events and its totals.
            assert len(await taken(finished)) == 2
            assert await finished.close() == speech.SessionTotals(320, 0, 0, 1, 0)

    run(main)


def test_closing_the_client_waits_for_a_session_no_longer_than_its_cancel_timeout(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def main() -> None:
        async with StreamServer().serving() as endpoint:
            stream = speech.SpeechClient(endpoint)
            session = stream.transcribe(
                TARGET, language="en", input_format=PCM, cancel_timeout=0.05
            )
            await session.open()
            monkeypatch.setattr(session, "_until_ended", lambda: asyncio.Event().wait())
            await stream.aclose()

    run(main)


def test_a_channel_the_caller_passes_is_used_and_left_open() -> None:
    async def main() -> None:
        fake = StreamServer()
        options: dict[str, Any] = {"language": "en", "input_format": PCM}
        async with fake.serving() as endpoint:
            channel = grpc.aio.insecure_channel(endpoint, options=client.CHANNEL_OPTIONS)
            for _ in range(2):
                stream = speech.SpeechClient(channel=channel)
                async with stream, stream.transcribe(TARGET, **options) as session:
                    await session.send_audio(FRAME)
            assert fake.calls == 2
            # Closing the client ends its sessions before it returns, open or not.
            stream = speech.SpeechClient(channel=channel)
            session, unopened = (stream.transcribe(TARGET, **options) for _ in range(2))
            await session.open()
            await stream.aclose()
            assert session._ended
            with pytest.raises(speech.SessionClosedError):
                await unopened.open()
            assert unopened._call is None
            # A channel its owner closed: no error of gRPC's reaches the caller.
            session = speech.SpeechClient(channel=channel).transcribe(TARGET, **options)
            await channel.close()
            for call in (session.open(), session.open(), session.send_audio(FRAME)):
                with pytest.raises(speech.SessionClosedError):
                    await call
            await session.cancel()
            assert await taken(session) == []

    run(main)
    both: dict[str, Any] = {"endpoint": "127.0.0.1:1", "channel": object()}
    for arguments in ({}, both):
        with pytest.raises(ValueError, match="an endpoint or a channel"):
            speech.SpeechClient(**arguments)


def test_a_closed_session_raises_the_closed_session_error_and_repeats_its_totals() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            await session.send_audio(FRAME)
        for call in (session.open(), session.send_audio(FRAME), session.finalize()):
            with pytest.raises(speech.SessionClosedError):
                await call
        assert await session.close() == speech.SessionTotals(320, 0, 0, 1, 0)
        await session.cancel()
        assert len(await taken(session)) == 2

    play(scenario)


@pytest.mark.parametrize(
    ("status", "error"),
    [
        (grpc.StatusCode.UNIMPLEMENTED, speech.PreconditionFailedError),
        (grpc.StatusCode.PERMISSION_DENIED, speech.ServerInternalError),
        (grpc.StatusCode.UNAUTHENTICATED, speech.ServerInternalError),
        (grpc.StatusCode.INTERNAL, speech.ServerInternalError),
    ],
)
def test_a_status_without_a_class_of_its_own_lands_where_the_errors_say(
    status: grpc.StatusCode, error: type[speech.SpeechError]
) -> None:
    failure = _session.failure_of(status, None, None)

    assert type(failure) is error
    assert vars(failure) == {
        "reason": None,
        "code": None,
        "grpc_status": status.name,
        "admitted": None,
    }
    assert not isinstance(failure, (TransportError, RequestTimeoutError))


def test_a_session_is_obtained_from_the_client_and_not_constructed() -> None:
    with pytest.raises(TypeError, match="obtained from a SpeechClient"):
        speech.SpeechToTextSession()
    assert list(inspect.signature(speech.SpeechToTextSession).parameters) == []


_FOREIGN = ("grpc", "google", "tensorplate.stream")


def _foreign(hint: Any) -> bool:
    """Whether a type hint or a default names, at any depth, a gRPC or a generated type."""
    module = getattr(hint, "__module__", None) or ""
    named = any(module == root or module.startswith(root + ".") for root in _FOREIGN)
    return named or any(_foreign(inner) for inner in typing.get_args(hint))


def _surface(exported: type) -> Iterator[tuple[str, Any]]:
    """The functions a caller reaches on `exported`: public ones, inherited ones and dunders."""
    for name, member in inspect.getmembers(exported):
        if name.startswith("_") and not name.endswith("__"):
            continue
        function = member.fget if isinstance(member, property) else member
        if inspect.isfunction(function) and function.__module__.startswith("tensorplate."):
            yield name, function


def test_the_public_names_are_the_sdks_own_types() -> None:
    assert not _foreign(speech.SpeechToTextEvent)
    assert _foreign(pb.Open | None)
    assert _foreign(type(grpc.StatusCode.OK))
    for name in speech.__all__:
        exported = getattr(speech, name)
        if not isinstance(exported, type):
            continue
        assert exported.__module__.startswith("tensorplate.speech."), name
        named = dict(typing.get_type_hints(exported))
        for member, function in _surface(exported):
            for part, hint in typing.get_type_hints(function).items():
                named[f"{member}({part})"] = hint
            for parameter in inspect.signature(function).parameters.values():
                if parameter.default is not inspect.Parameter.empty:
                    named[f"{member}({parameter.name}=)"] = type(parameter.default)
        assert [where for where, hint in named.items() if _foreign(hint)] == [], name
    assert issubclass(speech.SpeechError, TensorPlateError)
