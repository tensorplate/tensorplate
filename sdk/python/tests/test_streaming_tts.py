"""Text-to-speech sessions of the streaming speech client, against ``stream_server.py``.

As in ``test_streaming_client.py``, every session runs over a loopback gRPC
channel against a server that refuses what the schema does not allow. A
server that breaks a rule is the recorded session with one frame changed.
"""

from __future__ import annotations

import asyncio
import os
from collections.abc import Awaitable, Callable
from typing import Any

import pytest

from tensorplate.errors import ErrorCode, MissingDependencyError

try:
    import tensorplate.speech as speech
except MissingDependencyError as missing:
    if os.environ.get("TENSORPLATE_REQUIRE_SPEECH_EXTRAS") == "1":
        raise
    pytest.skip(str(missing), allow_module_level=True)

from stream_server import REPOSITORY, Playback, StreamServer, frames, recorded
from tensorplate.speech import tts
from tensorplate.speech._buffer import EventBuffer
from tensorplate.stream.v1 import session_pb2 as pb
from test_streaming_client import at_once, run, turns

TARGET = speech.Target("synthetic-deployment", 7, "sha256:" + "0" * 64)
PCM = speech.AudioFormat("pcm_s16le", 24000)
TEXT = "synthetic sentence"
# 20 ms of PCM, and the two seconds of it a session holds: one hundred chunks.
CHUNK = 960
HELD = 96_000
# Ten seconds of audio: five hundred chunks.
LONG = 240_000
# A stall time no run reaches, for a test that leaves the reader waiting and is not about
# the stall timer.
NO_STALL_MS = 3_600_000
Scenario = Callable[[StreamServer, speech.TextToSpeechSession], Awaitable[Any]]


def play(scenario: Scenario, server: StreamServer | None = None, **options: Any) -> Any:
    """Run `scenario` with a served `server` and a session on it that is not yet open."""
    fake = server or StreamServer()
    options = {"language": "en", "voice": "synthetic_voice", **options}

    async def main() -> Any:
        async with fake.serving() as endpoint, speech.SpeechClient(endpoint) as stream:
            return await scenario(fake, stream.synthesize(TARGET, **options))

    return run(main)


def tone(samples: int) -> bytes:
    """The audio the server sends for `samples` of a segment, from its start."""
    return (b"\x00\x10\x00\xf0" * samples)[: 2 * samples]


async def taken(session: speech.TextToSpeechSession) -> list[Any]:
    return [event async for event in session]


async def collect(session: speech.TextToSpeechSession, events: list[Any]) -> None:
    """Take the session's events into `events`, which keeps them when the session fails."""
    async for event in session:
        events.append(event)


async def read(session: speech.TextToSpeechSession, count: int) -> None:
    """Return once the session has read `count` server events."""
    while session._received < count and not session._ended:
        await asyncio.sleep(0)


async def settled(fake: StreamServer, session: speech.TextToSpeechSession) -> int:
    """Return how many chunks of its segment the server has written, once it writes no more.

    A server whose write the transport holds back takes no step. It still
    reads, so a Ping it has read with no write started meanwhile is the
    proof, asked again until it holds.
    """
    live = fake._session
    assert live is not None
    started = -1
    while live.out != started:
        started = live.out
        await ping(fake, session)
    return int(live.chunks)


async def ping(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
    """Send a Ping and return once the server has read it, taken or not."""
    arrived = fake.arrived
    await session._send(pb.ClientEvent(ping=pb.Ping()))
    await fake.until(lambda: fake.arrived > arrived)


async def held(fake: StreamServer, session: speech.TextToSpeechSession) -> int:
    """Send one segment of LONG and take nothing; return the chunks the server wrote of it.

    The session reads Ready, the segment's two Accepted and 101 chunks, and
    waits with the last of them for room.
    """
    await session.send_text(TEXT)
    await read(session, 104)
    return await settled(fake, session)


def test_text_within_the_credit_then_finalize_returns_the_synthesis_and_its_audio() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        async with session:
            assert session.session_id == "synthetic-session-1"
            assert session.output_format == PCM
            limits = session.limits
            assert limits.text_to_speech == speech.TextToSpeechLimits(512, 10000, 4096, 120000)
            assert (limits.speech_to_text, limits.finalize_deadline_ms) == (None, 120000)
            reader = asyncio.create_task(taken(session))
            assert await session.send_text("synthetic sentence one") == 1
            assert await session.send_text("synthetic sentence two") == 2
            done = await session.finalize()
            totals = await session.close()
        assert done == speech.SynthesisCompleted(2, 1920, finalize_sequence=4)
        chunks = [speech.AudioChunk(n, i, 480 * i, PCM, tone(480)) for n in (1, 2) for i in (0, 1)]
        assert await reader == [
            *chunks[:2],
            speech.SegmentCompleted(1, 960, 2),
            *chunks[2:],
            speech.SegmentCompleted(2, 960, 2),
            done,
        ]
        assert (chunks[0].sample_count, "data" in repr(chunks[0])) == (480, False)
        assert totals == speech.SessionTotals(0, 44, 1920, 0, 2)
        assert fake.bodies() == ["open", "text_segment", "text_segment", "finalize"]
        assert fake.received[0].open == recorded("tts_open").open
        assert session.unknown_events == 0

    play(scenario, trace_id="synthetic-trace-1")


async def recorded_session(playback: Playback) -> tuple[list[Any], Any, Any]:
    """Send what the recorded client sent; return the events, the Finalize's answer, the totals."""
    events: list[Any] = []
    async with playback.serving() as endpoint, speech.SpeechClient(endpoint) as stream:
        session = stream.synthesize(
            TARGET, language="en", voice="synthetic_voice", trace_id="synthetic-trace-1"
        )
        async with session:
            tasks: list[asyncio.Task[Any]] = [asyncio.create_task(collect(session, events))]
            try:
                for name in ("tts_text_1", "tts_text_2"):
                    segment = recorded(name).text_segment
                    await session.send_text(segment.text, turn_id=segment.turn_id)
                tasks.append(asyncio.create_task(session.finalize()))
                # The recording answers the Finalize after the half-close.
                while len(playback.received) < 4 and not session._ended:
                    await asyncio.sleep(0)
                totals = await session.close()
                answer = await tasks[1]
                await tasks[0]
                return events, answer, totals
            finally:
                # Whatever failed, no task is left waiting on a session that is still open.
                for task in tasks:
                    task.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)


def test_the_recorded_text_to_speech_session_is_what_the_client_sends_and_reads() -> None:
    session_frames = frames("tts_session")
    sent = [(name, encoded) for name, kind, encoded in session_frames if kind == "ClientEvent"]
    playback = Playback("tts_session")

    events, answer, totals = run(lambda: recorded_session(playback))

    assert playback.received == [recorded(name) for name, _ in sent]
    assert [event.SerializeToString() for event in playback.received] == [e for _, e in sent]
    assert events == [
        speech.AudioChunk(1, 0, 0, PCM, tone(480)),
        speech.AudioChunk(1, 1, 480, PCM, tone(480)),
        speech.SegmentCompleted(1, 960, 2),
        speech.AudioChunk(2, 0, 0, PCM, tone(480)),
        speech.AudioChunk(2, 1, 480, PCM, tone(200)),
        speech.SegmentCompleted(2, 680, 2),
        speech.SynthesisCompleted(2, 1640, 4),
    ]
    assert answer == events[-1]
    assert events[0].data == recorded("tts_chunk_1_1").audio_chunk.data
    assert totals == speech.SessionTotals(0, 44, 1640, 0, 2)


# One change to a recorded server frame: the frame, a dotted path in it, and the value the
# field takes; ``None`` clears the field, and LEFT_OUT leaves the whole frame out.
Edit = tuple[str, str, Any]
LEFT_OUT = object()


def altered(*edits: Edit) -> Playback:
    """The recorded text-to-speech session with `edits` made to its server frames."""

    def change(frame: str) -> Callable[[Any], None]:
        def apply(event: Any) -> None:
            for name, path, value in edits:
                if name != frame:
                    continue
                *parents, field = path.split(".")
                target = event
                for parent in parents:
                    target = getattr(target, parent)
                if value is None:
                    target.ClearField(field)
                else:
                    setattr(target, field, value)

        return apply

    left_out = {name for name, _, value in edits if value is LEFT_OUT}
    return Playback(
        "tts_session",
        {name: None if name in left_out else change(name) for name, _, _ in edits},
    )


_READY, _CREDIT = "ready.output_format", "input_credit.waiting_segments"
_BOUNDS = "ready.limits.text_to_speech"
_CHUNK, _DONE, _ANSWER = "audio_chunk", "segment_completed", "synthesis_completed"
_TOTALS = "session_closed.totals"
_SECOND_UNFINISHED: Edit = ("tts_segment_2", "", LEFT_OUT)


@pytest.mark.parametrize(
    ("what", "edits"),
    [
        ("no text bounds", [("tts_ready", _BOUNDS, None)]),
        ("no text bounds", [("tts_ready", f"{_BOUNDS}.max_segment_text_bytes", 0)]),
        ("no text bounds", [("tts_ready", f"{_BOUNDS}.max_synthesis_text_bytes", 0)]),
        ("no output stall time", [("tts_ready", "ready.limits.output_stall_timeout_ms", 0)]),
        ("no output format", [("tts_ready", _READY, None)]),
        ("no output format", [("tts_ready", f"{_READY}.encoding", 7)]),
        ("no output format", [("tts_ready", f"{_READY}.sample_rate_hz", 0)]),
        ("no output format", [("tts_ready", f"{_READY}.channels", 0)]),
        ("no waiting-segment budget", [("tts_ready", f"ready.status.{_CREDIT}", None)]),
        ("no waiting-segment budget", [("tts_accepted_2", f"accepted.{_CREDIT}", None)]),
        ("another format", [("tts_chunk_1_1", f"{_CHUNK}.format.sample_rate_hz", 16000)]),
        ("not the oldest unfinished", [("tts_chunk_1_1", f"{_CHUNK}.segment_id", 2)]),
        ("not the oldest unfinished", [("tts_chunk_1_2", f"{_CHUNK}.segment_id", 2)]),
        ("out of its place", [("tts_chunk_1_2", f"{_CHUNK}.chunk_index", 2)]),
        ("out of its place", [("tts_chunk_1_2", f"{_CHUNK}.segment_sample_offset", 479)]),
        ("not 20 ms", [("tts_chunk_1_1", f"{_CHUNK}.data", tone(481))]),
        ("not 20 ms", [("tts_chunk_1_1", f"{_CHUNK}.data", tone(480)[:-1])]),
        ("not 20 ms", [("tts_chunk_1_1", f"{_CHUNK}.data", b"")]),
        (
            "not 20 ms",
            [
                ("tts_chunk_1_1", f"{_CHUNK}.data", tone(200)),
                ("tts_chunk_1_2", f"{_CHUNK}.segment_sample_offset", 200),
            ],
        ),
        ("not the chunks read", [("tts_segment_1", f"{_DONE}.total_samples", 961)]),
        ("not the chunks read", [("tts_segment_1", f"{_DONE}.chunk_count", 3)]),
        ("not the chunks read", [("tts_segment_1", f"{_DONE}.segment_id", 2)]),
        (
            # A completion that counts nothing, for a segment no audio came of.
            "not the chunks read",
            [
                ("tts_chunk_2_1", "", LEFT_OUT),
                ("tts_chunk_2_2", "", LEFT_OUT),
                ("tts_segment_2", f"{_DONE}.total_samples", None),
                ("tts_segment_2", f"{_DONE}.chunk_count", None),
            ],
        ),
        (
            # An answer that counts what was completed, with the second segment unfinished.
            "before a segment's completion",
            [
                _SECOND_UNFINISHED,
                ("tts_synthesis_completed", f"{_ANSWER}.segment_count", 1),
                ("tts_synthesis_completed", f"{_ANSWER}.total_samples", 960),
            ],
        ),
        (
            "not the segments completed",
            [("tts_synthesis_completed", f"{_ANSWER}.segment_count", 3)],
        ),
        (
            "not the segments completed",
            [("tts_synthesis_completed", f"{_ANSWER}.total_samples", 1)],
        ),
        ("not waiting", [("tts_synthesis_completed", f"{_ANSWER}.finalize_sequence", 3)]),
        ("not waiting", [("tts_synthesis_completed", f"{_ANSWER}.finalize_sequence", None)]),
        ("closing totals", [("tts_closed", f"{_TOTALS}.output_audio_samples", 1641)]),
        ("closing totals", [("tts_closed", f"{_TOTALS}.segments_completed", 3)]),
        (
            # Totals that count what was completed, with the second segment unfinished.
            "left without its completion",
            [
                _SECOND_UNFINISHED,
                ("tts_synthesis_completed", "", LEFT_OUT),
                ("tts_closed", f"{_TOTALS}.segments_completed", 1),
            ],
        ),
    ],
)
def test_a_server_that_breaks_a_text_to_speech_rule_ends_the_session_with_a_protocol_error(
    what: str, edits: list[Edit]
) -> None:
    with pytest.raises(speech.StreamProtocolError, match=what) as caught:
        run(lambda: recorded_session(altered(*edits)))

    assert caught.value.admitted is (edits[0][0] != "tts_ready" or None)


def test_an_edit_that_changes_nothing_leaves_the_recorded_session_whole() -> None:
    # The control for the test above: its edits reach the client, and only they end the session.
    playback = altered(("tts_chunk_1_2", f"{_CHUNK}.chunk_index", 1))

    events, _, totals = run(lambda: recorded_session(playback))

    assert (len(events), totals.segments_completed) == (7, 2)


def test_a_segment_is_refused_before_it_is_sent_when_the_server_would_refuse_it() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        async with session:
            reader = asyncio.create_task(taken(session))
            for text in ("", " \t\n", "\u00a0\u3000"):
                with pytest.raises(ValueError, match="not only whitespace"):
                    await session.send_text(text)
            # The bound is on bytes of UTF-8: 257 characters here, 514 bytes.
            for text in ("x" * 513, "\u00e9" * 257):
                with pytest.raises(ValueError, match="at most 512 bytes"):
                    await session.send_text(text)
            for turn in ("t" * 65, "tur\u00f1", "turn\n"):
                with pytest.raises(ValueError, match="turn id"):
                    await session.send_text(TEXT, turn_id=turn)
            with pytest.raises(ValueError, match="surrogates not allowed"):
                await session.send_text(chr(0xD800))
            for other in (b"bytes", None, 7):
                with pytest.raises(TypeError, match="text is a str"):
                    await session.send_text(other)  # type: ignore[arg-type]
            assert await session.send_text("x" * 400, turn_id="t" * 64) == 1
            with pytest.raises(ValueError, match="at most 700 bytes of UTF-8 together"):
                await session.send_text("x" * 301)
            assert await session.send_text("x" * 300) == 2
            await session.finalize()
            # The bound on the text between two Finalizes starts again.
            assert await session.send_text("\u00e9" * 256) == 3
            assert (await session.close()).input_text_bytes == 1212
            await reader
        sent = [event.text_segment for event in fake.received if event.HasField("text_segment")]
        assert [(s.segment_id, len(s.text), s.turn_id) for s in sent] == [
            (1, 400, "t" * 64),
            (2, 300, ""),
            (3, 256, ""),
        ]

    play(scenario, StreamServer(max_synthesis_text_bytes=700))


def test_text_no_session_would_send_is_refused_at_once_behind_an_unanswered_finalize() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        async with session:
            reader = asyncio.create_task(taken(session))
            await session.send_text(TEXT)
            finalizing = asyncio.create_task(session.finalize())
            # The answer is held back, and the Finalize holds the input gate until it comes.
            await fake.until(lambda: "finalize" in fake.bodies())
            assert session._input_gate.locked()
            for text in ("", " \t\n", chr(0xD800)):
                assert isinstance(await at_once(session.send_text(text)), ValueError)
            refusal = await at_once(session.send_text(TEXT, turn_id="turn\n"))
            assert isinstance(refusal, ValueError)
            assert "turn id" in str(refusal)
            assert isinstance(await at_once(session.send_text(7)), TypeError)  # type: ignore[arg-type]
            fake.start()
            fake.speak(480)
            fake.complete()
            assert (await finalizing).segment_count == 1
        await reader
        assert fake.bodies() == ["open", "text_segment", "finalize"]

    play(scenario, StreamServer(hold_synthesis=True))


def test_a_segment_larger_than_the_byte_credit_is_refused_before_it_is_sent() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        async with session:
            with pytest.raises(ValueError, match="larger than the session's input credit"):
                await session.send_text("x" * 301)
        assert fake.bodies() == ["open"]

    play(scenario, StreamServer(text_credit=300))


async def barrier(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
    """Return once the server has taken everything the session wrote before this call."""
    pings = fake.bodies().count("ping")
    await session._send(pb.ClientEvent(ping=pb.Ping()))
    await fake.until(lambda: fake.bodies().count("ping") > pings or not fake.live)


def test_a_sender_waits_for_a_waiting_place_and_resumes_when_a_segment_starts() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        async with session:
            reader = asyncio.create_task(taken(session))
            sender = asyncio.create_task(send(session, 4))
            for count in (2, 3):
                await texts(fake, count)
                await turns()
                await barrier(fake, session)
                assert fake.bodies().count("text_segment") == count
                assert not sender.done()
                # The segment started leaves its waiting place to the next.
                fake.start()
                fake.speak(480)
                fake.complete()
            assert await sender == [1, 2, 3, 4]
            await texts(fake, 4)
            for _ in range(2):
                fake.start()
                fake.speak(480)
                fake.complete()
            assert (await session.close()).segments_completed == 4
            await reader

    play(scenario, StreamServer(hold_synthesis=True))


async def texts(fake: StreamServer, count: int) -> None:
    """Return once the server has taken `count` text segments."""
    await fake.until(lambda: fake.bodies().count("text_segment") == count)


async def send(session: speech.TextToSpeechSession, count: int, text: str = TEXT) -> list[int]:
    return [await session.send_text(text) for _ in range(count)]


def test_a_sender_waits_for_byte_credit_with_a_waiting_place_free() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        await session.open()
        sender = asyncio.create_task(send(session, 2, "x" * 400))
        await fake.until(lambda: "text_segment" in fake.bodies())
        await turns()
        await barrier(fake, session)
        assert fake.bodies().count("text_segment") == 1
        assert (session._segments_waiting, session._segment_limit) == (1, 2)
        assert not sender.done()
        fake.start()
        assert await sender == [1, 2]
        await session.cancel()

    play(scenario, StreamServer(text_credit=600, hold_synthesis=True))


def test_text_after_a_cancelled_finalize_waits_for_its_answer() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        async with session:
            reader = asyncio.create_task(taken(session))
            await session.send_text(TEXT)
            finalize = asyncio.create_task(session.finalize())
            await fake.until(lambda: "finalize" in fake.bodies())
            finalize.cancel()
            await asyncio.gather(finalize, return_exceptions=True)
            sender = asyncio.create_task(session.send_text(TEXT))
            await turns()
            await barrier(fake, session)
            assert fake.bodies().count("text_segment") == 1
            fake.start()
            fake.speak(480)
            fake.complete()
            assert await sender == 2
            await texts(fake, 2)
            fake.start()
            fake.speak(480)
            fake.complete()
            assert (await session.finalize()).segment_count == 1
        assert [type(event) for event in await reader].count(speech.SynthesisCompleted) == 2

    play(scenario, StreamServer(hold_synthesis=True))


def test_a_sender_cancelled_before_its_write_leaves_no_segment_behind() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        async with session:
            reader = asyncio.create_task(taken(session))
            async with session._write_lock:
                sender = asyncio.create_task(session.send_text("x" * 400))
                await turns()
                sender.cancel()
                await asyncio.gather(sender, return_exceptions=True)
            # Neither an id nor the 400 bytes were used up: 400 more fit the 700 allowed.
            assert await session.send_text("x" * 400) == 1
            assert (await session.finalize()).segment_count == 1
        assert fake.bodies().count("text_segment") == 1
        assert len(await reader) == 4

    play(scenario, StreamServer(max_synthesis_text_bytes=700))


def test_closing_without_a_finalize_delivers_every_segment_and_no_synthesis_completed() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        async with session:
            reader = asyncio.create_task(taken(session))
            await session.send_text(TEXT)
            await session.send_text(TEXT)
        events = await reader
        assert [type(event) for event in events] == [
            *[speech.AudioChunk] * 3,
            speech.SegmentCompleted,
            *[speech.AudioChunk] * 3,
            speech.SegmentCompleted,
        ]
        # 1,000 samples: two chunks of 20 ms and a last one of 40 samples.
        assert [len(event.data) for event in events[:3]] == [CHUNK, CHUNK, 80]
        assert b"".join(event.data for event in events[:3]) == tone(1000)
        with pytest.raises(speech.SessionClosedError):
            await session.send_text(TEXT)

    play(scenario, StreamServer(segment_samples=1000))


def test_a_reader_that_stops_holds_two_seconds_of_audio_and_holds_the_server_back() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        async with session:
            written = await held(fake, session)
            assert session._received == 104
            assert (session._events.audio_used, session._events.used) == (HELD, 0)
            # Of the segment's 500 chunks the server wrote the 101 the session read and
            # those the transport took, within two HTTP/2 windows, and no more.
            assert 101 <= written < 500
            assert (written - 101) * CHUNK <= 2 * 65_535
            # Taking one chunk makes room for the one that waited, and one more is read.
            first = await anext(session)
            await read(session, 105)
            await turns()
            assert (session._received, session._events.audio_used) == (105, HELD)
            reader = asyncio.create_task(taken(session))
            totals = await session.close()
            events = [first, *await reader]
        chunks = events[:-1]
        assert [chunk.chunk_index for chunk in chunks] == list(range(500))
        assert [chunk.segment_sample_offset for chunk in chunks] == list(range(0, LONG, 480))
        assert b"".join(chunk.data for chunk in chunks) == tone(LONG)
        assert events[-1] == speech.SegmentCompleted(1, LONG, 500)
        assert totals.output_audio_samples == LONG

    play(scenario, StreamServer(segment_samples=LONG))
    assert PCM.sample_width * PCM.sample_rate_hz * tts.PLAYBACK_BUFFER_MS // 1000 == HELD


def test_a_caller_that_takes_on_the_turn_the_reader_stops_is_sent_the_rest_of_the_audio() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        async with session:
            await session.send_text(TEXT)
            # The reader has this moment found the buffer full, and no turn of the loop
            # passes before the takes: none of them waits.
            await read(session, 104)
            events = [await anext(session) for _ in range(100)]
            await turns()
            # The chunk that waited for room was admitted.
            assert session._events.audio_used > 0
            reader = asyncio.create_task(taken(session))
            totals = await session.close()
            events += await reader
        assert [type(event) for event in events].count(speech.AudioChunk) == 500
        assert totals.output_audio_samples == LONG

    play(scenario, StreamServer(segment_samples=LONG, output_stall_timeout_ms=NO_STALL_MS))


def test_the_playback_buffer_is_two_seconds_of_whatever_format_ready_declares() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        await session.open()
        assert session.output_format == speech.AudioFormat("mulaw", 8000, channels=2)
        await session.send_text(TEXT)
        # 20 ms is 160 samples of two bytes: one hundred chunks of 320 bytes are held.
        await read(session, 104)
        await settled(fake, session)
        assert (session._received, session._events.audio_used) == (104, 32_000)
        await session.cancel()

    play(
        scenario,
        StreamServer(output_format=(pb.AUDIO_ENCODING_MULAW, 8000, 2), segment_samples=LONG),
    )


def test_a_sender_held_back_by_audio_nobody_takes_resumes_when_it_is_taken() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        async with session:
            # The first segment is being sent and two wait: a fourth has no place.
            assert await send(session, 3) == [1, 2, 3]
            sender = asyncio.create_task(session.send_text(TEXT))
            await read(session, 104)
            await settled(fake, session)
            assert fake.bodies().count("text_segment") == 3
            assert not sender.done()
            reader = asyncio.create_task(taken(session))
            assert await sender == 4
            assert (await session.close()).segments_completed == 4
            assert len(await reader) == 4 * 501

    play(scenario, StreamServer(segment_samples=LONG))


def test_the_servers_slow_consumer_end_follows_the_audio_the_session_held() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        await session.open()
        written = await held(fake, session)
        # What the server's own limit on undelivered output does, once the reader has stopped.
        fake.end("slow_consumer")
        events: list[Any] = []
        with pytest.raises(speech.ResourceExhaustedError) as caught:
            await collect(session, events)
        failure = caught.value
        assert (failure.reason, failure.code) == ("slow_consumer", ErrorCode.RESOURCE_EXHAUSTED)
        assert (failure.grpc_status, failure.admitted) == ("RESOURCE_EXHAUSTED", True)
        # Every chunk the server wrote before it gave up, in order, and no completion.
        assert [event.chunk_index for event in events] == list(range(len(events)))
        assert 101 <= written <= len(events) < 500
        assert fake.closed[0].totals.output_audio_samples == 480 * len(events)
        with pytest.raises(speech.ResourceExhaustedError):
            await session.send_text(TEXT)

    play(scenario, StreamServer(segment_samples=LONG))


def test_audio_left_untaken_for_the_servers_stall_time_ends_the_session() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        await session.open()
        await session.send_text(TEXT)
        with pytest.raises(speech.ResourceExhaustedError, match="left unread") as caught:
            await session.close()
        failure = caught.value
        assert (failure.admitted, failure.grpc_status, failure.reason) == (True, None, None)
        events: list[Any] = []
        with pytest.raises(speech.ResourceExhaustedError):
            await collect(session, events)
        assert len(events) == 100
        await fake.until(lambda: fake.live == 0)

    play(scenario, StreamServer(segment_samples=LONG, output_stall_timeout_ms=50))


def test_cancel_drops_the_audio_held_and_stops_the_server_where_it_is() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        await session.open()
        await held(fake, session)
        await session.cancel()
        assert await taken(session) == []
        assert session._events.audio_used == 0
        await fake.until(lambda: fake.live == 0)
        closed = fake.closed[0]
        assert closed.cause.reason == pb.FAILURE_REASON_CLIENT_CANCELLED
        assert 0 < closed.totals.output_audio_samples < LONG
        assert closed.totals.segments_completed == 0

    play(scenario, StreamServer(segment_samples=LONG))


def test_a_session_the_server_ends_in_a_segment_raises_the_cause_after_its_audio() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        await session.open()
        await session.send_text(TEXT)
        await fake.until(lambda: "text_segment" in fake.bodies())
        fake.start()
        fake.speak(480)
        fake.end("worker_shutdown")
        events: list[Any] = []
        # The segment has no completion and the totals count none: the cause is the error.
        with pytest.raises(speech.ServiceUnavailableError) as caught:
            await collect(session, events)
        assert caught.value.reason == "worker_shutdown"
        assert events == [speech.AudioChunk(1, 0, 0, PCM, tone(480))]
        for call in (session.send_text(TEXT), session.finalize(), session.close()):
            with pytest.raises(speech.ServiceUnavailableError):
                await call

    play(scenario, StreamServer(hold_synthesis=True))


@pytest.mark.parametrize("fault", ["unknown_event", "transcript"])
def test_a_server_event_a_text_session_does_not_know_is_passed_over_and_counted(
    fault: str,
) -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        async with session:
            reader = asyncio.create_task(taken(session))
            await session.send_text(TEXT)
            assert (await session.finalize()).segment_count == 1
            assert session.unknown_events == 1
        assert len(await reader) == 4

    play(scenario, StreamServer(fault=fault))


def test_audio_of_no_segment_ends_the_session_with_a_protocol_error() -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        await session.open()
        with pytest.raises(speech.StreamProtocolError, match="not the oldest unfinished"):
            await taken(session)

    play(scenario, StreamServer(fault="audio_chunk"))


@pytest.mark.parametrize(
    ("options", "error"),
    [
        ({"voice": "another_voice"}, speech.PreconditionFailedError),
        ({"language": "xx"}, speech.PreconditionFailedError),
        ({"voice": ""}, speech.InvalidEventError),
    ],
)
def test_a_refused_text_to_speech_open_raises_the_class_of_its_status(
    options: dict[str, Any], error: type[speech.SpeechError]
) -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        with pytest.raises(speech.SpeechError) as caught:
            await session.open()
        assert (type(caught.value), caught.value.admitted) == (error, None)
        with pytest.raises(speech.SessionClosedError):
            session.output_format  # noqa: B018

    play(scenario, **options)


def test_the_event_buffer_bounds_audio_and_other_events_apart() -> None:
    async def main() -> None:
        buffer: EventBuffer[str] = EventBuffer(10)
        # No audio bound is set yet: one chunk is admitted, since none is held.
        await buffer.put("first", 8, audio=True)
        buffer.bound_audio(20)
        await buffer.put("second", 12, audio=True)
        blocked = asyncio.create_task(buffer.put("third", 1, audio=True))
        await turns()
        assert not blocked.done()
        # Audio at its bound holds no other event back, and the reverse.
        await buffer.put("event", 10)
        assert (buffer.audio_used, buffer.used) == (20, 10)
        late = asyncio.create_task(buffer.put("late", 1))
        await turns()
        assert not late.done()
        assert await buffer.get() == "first"
        await blocked
        assert not late.done()
        assert (buffer.audio_used, buffer.used) == (13, 10)
        assert [await buffer.get(), await buffer.get(), await buffer.get()] == [
            "second",
            "event",
            "third",
        ]
        await late
        assert (buffer.audio_used, buffer.used) == (0, 1)
        buffer.unbound()
        for _ in range(2):
            await buffer.put("more", 100, audio=True)
            await buffer.put("more", 100)
        assert (buffer.audio_used, buffer.used) == (200, 201)
        buffer.end(discard=True)
        assert (buffer.audio_used, buffer.used) == (0, 0)

    run(main)


def documented(**server: Any) -> tuple[dict[str, Any], StreamServer]:
    """The text-to-speech example of the SDK's page, and a server for the deployment it names."""
    page = (REPOSITORY / "docs/sdk/python.md").read_text(encoding="utf-8")
    section = page[page.index("### Text-to-speech sessions") :]
    example: dict[str, Any] = {}
    exec(section.split("```python\n", 1)[1].split("```", 1)[0], example)
    fake = StreamServer(
        deployment_id="my-deployment",
        generation=0,
        descriptor_digest="",
        voices=("my-voice",),
        **server,
    )
    return example, fake


def test_the_documented_example_plays_audio_while_it_sends() -> None:
    # Four segments of ten seconds: more audio than a session holds, and more segments than
    # may wait, so the example ends only if it takes audio while it sends.
    example, fake = documented(segment_samples=LONG)
    played: list[bytes] = []

    async def play_chunk(data: bytes) -> None:
        played.append(data)

    async def main() -> Any:
        async with fake.serving() as endpoint:
            return await example["speak"](endpoint, [TEXT] * 4, play_chunk)

    assert run(main) == 4 * LONG
    assert b"".join(played) == tone(LONG) * 4
    assert fake.bodies() == ["open", *["text_segment"] * 4, "finalize"]


def test_the_documented_example_raises_its_players_error_when_playing_fails() -> None:
    # Four segments of ten seconds and no stall timer to speak of: an example that left
    # its sender waiting behind the audio nobody takes would not return.
    example, fake = documented(segment_samples=LONG, output_stall_timeout_ms=NO_STALL_MS)
    failure = RuntimeError("the audio device failed")

    async def play_chunk(data: bytes) -> None:
        raise failure

    async def main() -> None:
        async with fake.serving() as endpoint:
            with pytest.raises(RuntimeError) as caught:
                await example["speak"](endpoint, [TEXT] * 4, play_chunk)
            assert caught.value is failure
            await fake.until(lambda: fake.live == 0)
            assert fake.closed[0].cause.reason == pb.FAILURE_REASON_CLIENT_CANCELLED

    # `run` fails on a task whose exception nobody took.
    run(main)


def test_the_documented_example_takes_its_players_outcome_when_the_session_fails() -> None:
    example, fake = documented(hold_synthesis=True)

    async def play_chunk(data: bytes) -> None:
        raise AssertionError("no audio is sent")

    async def main() -> None:
        async with fake.serving() as endpoint:
            running = asyncio.create_task(example["speak"](endpoint, [TEXT], play_chunk))
            await fake.until(lambda: "finalize" in fake.bodies())
            fake.end("worker_shutdown")
            with pytest.raises(speech.ServiceUnavailableError):
                await running

    # `run` fails on a task whose exception nobody took.
    run(main)


def test_a_text_to_speech_session_is_obtained_from_the_client_with_its_bounds() -> None:
    with pytest.raises(TypeError, match="obtained from a SpeechClient"):
        speech.TextToSpeechSession()

    async def main() -> None:
        async with speech.SpeechClient("127.0.0.1:1") as stream:
            options: dict[str, Any] = {"language": "en", "voice": "synthetic_voice"}
            default = stream.synthesize(TARGET, **options)
            assert (default._open_timeout, default._cancel_timeout) == (10.0, 5.0)
            given = stream.synthesize(
                TARGET, open_timeout=1.0, close_timeout=None, cancel_timeout=3.0, **options
            )
            assert (given._open_timeout, given._close_timeout, given._cancel_timeout) == (
                1.0,
                None,
                3.0,
            )
            await stream.aclose()
            with pytest.raises(speech.SessionClosedError, match="client is closed"):
                stream.synthesize(TARGET, **options)

    run(main)


@pytest.mark.parametrize(
    ("deadline_ms", "given", "bound"),
    [
        (120_000, {}, 150.0),
        (300_000, {}, 330.0),
        (300_000, {"close_timeout": 150.0}, 150.0),
        (300_000, {"close_timeout": None}, None),
    ],
)
def test_a_close_without_a_timeout_is_bounded_by_the_servers_finalize_deadline_and_a_margin(
    deadline_ms: int, given: dict[str, Any], bound: float | None, monkeypatch: pytest.MonkeyPatch
) -> None:
    async def scenario(fake: StreamServer, session: speech.TextToSpeechSession) -> None:
        bounds: list[float | None] = []
        within = session._within

        async def spy(work: Any, timeout: float | None) -> Any:
            bounds.append(timeout)
            return await within(work, timeout)

        monkeypatch.setattr(session, "_within", spy)
        async with session:
            assert session.limits.finalize_deadline_ms == deadline_ms
        # The open's bound, then the bound of the close that the exit made.
        assert bounds == [10.0, bound]

    play(scenario, StreamServer(synthesis_deadline_ms=deadline_ms), **given)
    assert tts.CLOSE_MARGIN_S == 30.0


def test_closing_the_client_ends_a_text_to_speech_session_made_from_it() -> None:
    fake = StreamServer(hold_synthesis=True)

    async def main() -> None:
        async with fake.serving() as endpoint:
            async with speech.SpeechClient(endpoint) as stream:
                session = stream.synthesize(TARGET, language="en", voice="synthetic_voice")
                await session.open()
                await session.send_text(TEXT)
            with pytest.raises(speech.SessionClosedError):
                await session.send_text(TEXT)
            assert await taken(session) == []
            await fake.until(lambda: fake.live == 0)

    run(main)
