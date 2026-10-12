"""The in-process stream server of ``stream_server.py``, held to the recorded frames.

The frames under ``test/contract/fixtures/stream/v1`` are the ones the C++
bindings are tested against. These tests drive the server with the generated
stub alone, so they say nothing about the SDK's client.
"""

from __future__ import annotations

import asyncio
import contextlib
import os
import re
from collections.abc import Callable
from typing import Any

import pytest

from tensorplate.errors import MissingDependencyError

try:
    import tensorplate.stream  # noqa: F401
except MissingDependencyError as missing:
    if os.environ.get("TENSORPLATE_REQUIRE_SPEECH_EXTRAS") == "1":
        raise
    pytest.skip(str(missing), allow_module_level=True)

import grpc
import grpc.aio

from stream_server import REPOSITORY, STATUS, StreamServer, end_cause, frames, recorded
from tensorplate.stream.v1 import session_pb2 as pb
from tensorplate.stream.v1 import session_pb2_grpc as rpc

Steps = list[tuple[Any, ...]]


def converse(
    server: StreamServer, *turns: tuple[list[Any], Steps], half_close: bool = True
) -> tuple[list[Any], Any]:
    """Run one call and return what the server sent and the final status.

    Each turn is client events to send and the steps to queue on the server
    once it has taken them; the last turn's events end with the half-close.
    """

    async def call_once() -> tuple[list[Any], Any]:
        async with server.serving() as endpoint, grpc.aio.insecure_channel(endpoint) as channel:
            call = rpc.SessionServiceStub(channel).Session()
            got: list[Any] = []
            sent = 0
            with contextlib.suppress(grpc.aio.AioRpcError):
                for events, steps in turns:
                    for event in events:
                        await call.write(event)
                    sent += len(events)
                    last = half_close and steps is turns[-1][1]
                    if last:
                        await call.done_writing()
                    if steps:
                        await server.until(_took(server, sent, last))
                    for step, *arguments in steps:
                        getattr(server, step)(*arguments)
                while (event := await call.read()) is not grpc.aio.EOF:
                    got.append(event)
            return got, await call.code()

    return asyncio.run(asyncio.wait_for(call_once(), 60))


def _took(server: StreamServer, count: int, half_closed: bool) -> Callable[[], bool]:
    return lambda: len(server.received) == count and server.half_closed == half_closed


def exchange(
    server: StreamServer, events: list[Any], *, half_close: bool = True, then: str = ""
) -> tuple[list[Any], Any]:
    """Send `events` on one call; `then` is a reason the server ends the session with after them."""
    return converse(
        server, (events, [("end", then)] if then else []), half_close=half_close and not then
    )


def test_the_recorded_speech_to_text_session_is_served_frame_for_frame() -> None:
    # The server has no recognizer, so this supplies what one decides and nothing else: each
    # transcript's text and intervals, the silence that ends the first utterance at sample
    # 480, and when audio is consumed and an Accepted goes out. Every other byte of the
    # twenty recorded server frames is the server's own, and none differs.
    server = StreamServer(hold_credit=True, hold_accepted=True, hold_finals=True)
    first = [(0, 30000, "synthetic"), (30000, 60000, " transcript")]
    second = [(75000, 100000, "synthetic"), (100000, 110000, " words")]
    turns: list[tuple[list[str], Steps]] = [
        (["stt_open"], []),
        (["stt_audio_1", "stt_audio_2"], [("accept",), ("partial", "synthetic")]),
        (
            ["stt_audio_3"],
            [
                ("consume", 320),
                ("accept",),
                ("partial", "synthetic transcript"),
                ("endpoint", 480),
                ("final", first),
                ("consume", 320),
                ("accept",),
            ],
        ),
        (
            ["stt_audio_4", "stt_audio_5", "stt_finalize_1"],
            [
                ("accept",),
                ("partial", "synthetic words"),
                ("endpoint",),
                ("final", second),
                ("consume", 240),
            ],
        ),
        (
            ["stt_audio_6", "stt_finalize_2"],
            [("accept",), ("endpoint",), ("final", []), ("consume", 160)],
        ),
        (
            ["stt_audio_7", "stt_audio_8"],
            [
                ("accept",),
                ("partial", "synthetic end"),
                ("endpoint",),
                ("final", [(135000, 160000, "synthetic end")]),
            ],
        ),
    ]
    session = frames("stt_session")
    client = [name for name, kind, _ in session if kind == "ClientEvent"]
    assert [name for names, _ in turns for name in names] == client

    got, status = converse(
        server, *(([recorded(name) for name in names], steps) for names, steps in turns)
    )

    assert status is grpc.StatusCode.OK
    expected = [(name, encoded) for name, kind, encoded in session if kind == "ServerEvent"]
    assert got == [recorded(name) for name, _ in expected]
    assert [event.SerializeToString() for event in got] == [encoded for _, encoded in expected]


def _open(**changes: Any) -> Any:
    opened = recorded("stt_open")
    for name, value in changes.items():
        if name in ("deployment_id", "generation"):
            setattr(opened.open.resolved, name, value)
        elif name == "sequence":
            opened.sequence = value
        else:
            setattr(opened.open, name, value)
    return opened


def _first(event: Any) -> Any:
    """`event` as the first of a call: sequence one and no session named."""
    event.sequence = 1
    event.ClearField("session_id")
    return event


@pytest.mark.parametrize(
    ("reason", "opened", "server"),
    [
        ("invalid_event", _open(sequence=2), StreamServer()),
        ("invalid_event", _first(recorded("stt_audio_1")), StreamServer()),
        ("invalid_event", _open(turn_id="t" * 65), StreamServer()),
        ("invalid_event", _open(turn_id="tur\u00f1"), StreamServer()),
        ("invalid_event", _open(turn_id="turn\n"), StreamServer()),
        ("invalid_event", _open(turn_id="turn\t"), StreamServer()),
        ("invalid_event", _open(turn_id="turn\x7f"), StreamServer()),
        ("invalid_event", _open(trace_id="t" * 65), StreamServer()),
        ("invalid_event", _open(trace_id="trac\u00e9"), StreamServer()),
        ("invalid_event", _open(trace_id="trace\n"), StreamServer()),
        ("backend_unsupported_capability", _open(language="xx"), StreamServer()),
        ("target_unresolved", recorded("open_selector"), StreamServer()),
        ("target_mismatch", _open(deployment_id="another-deployment"), StreamServer()),
        ("stale_generation", _open(generation=6), StreamServer()),
        ("admission_closed", _open(), StreamServer(admission_open=False)),
        ("session_count_limit", _open(), StreamServer(max_sessions=0)),
    ],
)
def test_an_open_is_refused_with_the_recorded_refusal_and_no_event(
    reason: str, opened: Any, server: StreamServer
) -> None:
    got, status = exchange(server, [opened])

    assert got == []
    assert status is STATUS[reason]
    assert server.refusals == [recorded(f"refused_{reason}")]
    assert server.closed == []


def test_an_opens_trace_and_turn_ids_are_held_to_the_length_the_schema_states() -> None:
    schema = REPOSITORY / "protocol/proto/tensorplate/stream/v1/session.proto"
    rule = re.search(
        r"// Optional, at most (\d+) ASCII characters each\.\n +string trace_id = 7;\n"
        r" +string turn_id = 8;\n",
        schema.read_text(encoding="utf-8"),
    )
    assert rule, "the schema no longer states one bound for an Open's trace and turn ids"
    bound = int(rule.group(1))

    for name in ("trace_id", "turn_id"):
        got, status = exchange(StreamServer(), [_open(**{name: "t" * bound})])
        assert (_bodies(got), status) == (["ready", "session_closed"], grpc.StatusCode.OK), name
        got, status = exchange(StreamServer(), [_open(**{name: "t" * (bound + 1)})])
        assert (got, status) == ([], grpc.StatusCode.INVALID_ARGUMENT), name


def _audio(sequence: int, offset: int, samples: int = 160, data: bytes | None = None) -> Any:
    audio = pb.Audio(data=data or bytes(samples), sample_count=samples, sample_offset=offset)
    return pb.ClientEvent(sequence=sequence, session_id=_SESSION, audio=audio)


_SESSION = "synthetic-session-1"
_SECOND_OPEN = pb.ClientEvent(sequence=2, session_id=_SESSION, open=recorded("stt_open").open)


def _ends(got: list[Any], name: str) -> None:
    closed, expected = got[-1].session_closed, recorded(name).session_closed
    assert (closed.state, closed.cause, closed.admission) == (
        expected.state,
        expected.cause,
        expected.admission,
    )


def test_input_above_the_credit_fails_the_session_as_recorded() -> None:
    server = StreamServer(hold_credit=True, max_frame_ms=320)
    # 8,000 bytes of credit: three frames of 2,560 fit and the fourth does not.
    frames = [_audio(2 + n, 2560 * n, 2560) for n in range(4)]

    got, status = exchange(server, [recorded("stt_open"), *frames], half_close=False)

    assert status is grpc.StatusCode.RESOURCE_EXHAUSTED
    assert [event.accepted.input_credit.bytes.used for event in got[1:-1]] == [2560, 5120, 7680]
    _ends(got, "failed_input_credit_exceeded")
    assert got[-1].session_closed.last_accepted_sequence == 4


@pytest.mark.parametrize(
    ("what", "event"),
    [
        ("a gap in the sequence", _audio(3, 0)),
        ("another session's id", pb.ClientEvent(sequence=2, session_id="other", ping=pb.Ping())),
        ("an offset that is not the samples sent", _audio(2, 160)),
        ("a sample count that is not the data's", _audio(2, 0, 160, data=bytes(80))),
        ("a frame above the bound", _audio(2, 0, 400)),
        ("a second Open", _SECOND_OPEN),
        ("a body the schema lacks", recorded("added_client_body")),
        ("text in a speech-to-text session", recorded("tts_text_1")),
    ],
)
def test_an_event_the_schema_does_not_allow_fails_the_session(what: str, event: Any) -> None:
    got, status = exchange(StreamServer(), [recorded("stt_open"), event], half_close=False)

    assert status is grpc.StatusCode.INVALID_ARGUMENT, what
    closed = got[-1].session_closed
    assert closed.state == pb.SESSION_STATE_FAILED
    assert closed.cause == recorded("refused_invalid_event").cause


def test_audio_after_a_short_frame_fails_the_session_unless_a_finalize_came_between() -> None:
    short, finalize = _audio(2, 0, 80), recorded("stt_finalize_1")
    finalize.sequence = 3

    got, status = exchange(StreamServer(), [recorded("stt_open"), short, _audio(3, 80)])
    assert status is grpc.StatusCode.INVALID_ARGUMENT

    got, status = exchange(StreamServer(), [recorded("stt_open"), short, finalize, _audio(4, 80)])
    assert status is grpc.StatusCode.OK
    assert got[-1].session_closed.totals.utterances_completed == 2


def test_a_short_frame_stays_the_last_across_an_automatic_endpoint() -> None:
    server = StreamServer(hold_finals=True)
    opening = ([recorded("stt_open"), _audio(2, 0, 80)], [("endpoint",), ("final",)])

    got, status = converse(server, opening, ([_audio(3, 80)], []), half_close=False)

    assert status is grpc.StatusCode.INVALID_ARGUMENT
    assert [event.WhichOneof("body") for event in got[2:]] == [
        "endpoint_detected",
        "final_transcript",
        "session_closed",
    ]


def _finalize(sequence: int) -> Any:
    return pb.ClientEvent(sequence=sequence, session_id=_SESSION, finalize=pb.Finalize())


def test_audio_before_the_answer_to_a_finalize_fails_the_session() -> None:
    server = StreamServer(hold_finals=True)
    events = [recorded("stt_open"), _audio(2, 0), _finalize(3), _audio(4, 160)]

    got, status = exchange(server, events, half_close=False)

    assert status is grpc.StatusCode.FAILED_PRECONDITION
    closed = got[-1].session_closed
    assert (closed.state, closed.cause) == (
        pb.SESSION_STATE_FAILED,
        end_cause("illegal_transition"),
    )
    assert closed.last_accepted_sequence == 2
    assert not any(event.HasField("final_transcript") for event in got)


def test_a_finalize_repeated_before_its_answer_has_no_answer_of_its_own() -> None:
    server = StreamServer(hold_finals=True)
    events = [recorded("stt_open"), _audio(2, 0), _finalize(3), _finalize(4)]

    got, status = converse(server, (events, [("final",)]))

    assert status is grpc.StatusCode.OK
    finals = [event.final_transcript for event in got if event.HasField("final_transcript")]
    assert [(final.utterance_id, final.finalize_sequence) for final in finals] == [(1, 3)]


def test_an_automatic_endpoints_final_names_no_finalize_and_precedes_a_finalizes_own() -> None:
    server = StreamServer(hold_finals=True)
    opening = ([recorded("stt_open"), _audio(2, 0)], [("endpoint", 100)])
    joined = ([_audio(3, 160), _finalize(4)], [("final",), ("final",)])

    got, status = converse(server, opening, joined)

    assert status is grpc.StatusCode.OK
    silence, client = pb.ENDPOINT_REASON_SILENCE, pb.ENDPOINT_REASON_CLIENT_FINALIZE
    task = [event for event in got if event.WhichOneof("body") not in ("ready", "accepted")]
    assert [event.WhichOneof("body") for event in task] == [
        "endpoint_detected",
        "final_transcript",
        "endpoint_detected",
        "final_transcript",
        "session_closed",
    ]
    endpoints = [task[0].endpoint_detected, task[2].endpoint_detected]
    assert [(e.utterance_id, e.end_sample_offset, e.reason) for e in endpoints] == [
        (1, 100, silence),
        (2, 320, client),
    ]
    finals = [task[1].final_transcript, task[3].final_transcript]
    assert [(f.utterance_id, f.end_sample_offset, f.finalize_sequence) for f in finals] == [
        (1, 100, 0),
        (2, 320, 4),
    ]
    assert task[4].session_closed.totals.utterances_completed == 2


def test_a_cancel_a_ping_and_a_timer_end_as_recorded() -> None:
    cancel = recorded("cancel")
    cancel.sequence = 3
    ping = recorded("ping")
    ping.sequence = 2

    got, status = exchange(StreamServer(), [recorded("stt_open"), ping, cancel], half_close=False)
    assert status is grpc.StatusCode.CANCELLED
    assert [event.WhichOneof("body") for event in got] == [
        "ready",
        "pong",
        "cancel_accepted",
        "session_closed",
    ]
    _ends(got, "closed_client_cancelled")

    got, status = exchange(
        StreamServer(), [recorded("stt_open")], half_close=False, then="idle_timeout"
    )
    assert status is grpc.StatusCode.DEADLINE_EXCEEDED
    _ends(got, "closed_idle_timeout")


def test_the_recorded_text_to_speech_session_is_served_frame_for_frame() -> None:
    # The server has no synthesizer, so this supplies what one decides and nothing else: when
    # each segment starts, how many samples it becomes, and when an Accepted goes out. Every
    # other byte of the twelve recorded server frames is the server's own, and none differs.
    server = StreamServer(hold_accepted=True, hold_synthesis=True)
    first = [("start",), ("accept",), ("speak", 960), ("complete",)]
    second = [("start",), ("accept",), ("speak", 680), ("complete",)]
    turns: list[tuple[list[str], Steps]] = [
        (["tts_open"], []),
        (["tts_text_1", "tts_text_2"], [("accept",), *first, *second]),
        (["tts_finalize"], []),
    ]
    session = frames("tts_session")
    client = [name for name, kind, _ in session if kind == "ClientEvent"]
    assert [name for names, _ in turns for name in names] == client

    got, status = converse(
        server, *(([recorded(name) for name in names], steps) for names, steps in turns)
    )

    assert status is grpc.StatusCode.OK
    expected = [(name, encoded) for name, kind, encoded in session if kind == "ServerEvent"]
    assert len(expected) == 12
    assert got == [recorded(name) for name, _ in expected]
    assert [event.SerializeToString() for event in got] == [encoded for _, encoded in expected]


def _voiced(**changes: Any) -> Any:
    opened = recorded("tts_open")
    for name, value in changes.items():
        if name == "input_format":
            opened.open.input_format.CopyFrom(value)
        else:
            setattr(opened.open, name, value)
    return opened


@pytest.mark.parametrize(
    ("reason", "opened"),
    [
        ("invalid_event", _voiced(voice="")),
        ("invalid_event", _voiced(voice="v" * 65)),
        ("invalid_event", _voiced(turn_id="t" * 33 + "\u00f1" * 33)),
        ("invalid_event", _voiced(input_format=recorded("stt_open").open.input_format)),
        ("backend_unsupported_capability", _voiced(voice="another_voice")),
        ("backend_unsupported_capability", _voiced(language="xx")),
    ],
)
def test_a_text_to_speech_open_is_refused_with_the_recorded_refusal(
    reason: str, opened: Any
) -> None:
    server = StreamServer()

    got, status = exchange(server, [_first(opened)])

    assert (got, status) == ([], STATUS[reason])
    assert server.refusals == [recorded(f"refused_{reason}")]


def _text(sequence: int, segment: int, text: str = "synthetic sentence", turn: str = "") -> Any:
    body = pb.TextSegment(segment_id=segment, text=text, turn_id=turn)
    return pb.ClientEvent(sequence=sequence, session_id=_SESSION, text_segment=body)


def _bodies(got: list[Any]) -> list[str | None]:
    return [event.WhichOneof("body") for event in got]


@pytest.mark.parametrize(
    ("what", "events", "server"),
    [
        ("audio in a text-to-speech session", [_audio(2, 0)], StreamServer()),
        ("an id that repeats the last", [_text(2, 5), _text(3, 5)], StreamServer()),
        ("an id below an earlier one", [_text(2, 5), _text(3, 4)], StreamServer()),
        ("text above the segment bound", [_text(2, 1, "x" * 513)], StreamServer()),
        (
            "text above the bound between two Finalizes",
            [_text(2, 1, "x" * 400), _text(3, 2, "x" * 400)],
            StreamServer(max_synthesis_text_bytes=700, hold_synthesis=True),
        ),
        ("a turn id above 64 characters", [_text(2, 1, turn="t" * 65)], StreamServer()),
        ("a turn id that is not ASCII", [_text(2, 1, turn="tur\u00f1")], StreamServer()),
    ],
)
def test_a_text_event_the_schema_does_not_allow_fails_the_session(
    what: str, events: list[Any], server: StreamServer
) -> None:
    got, status = exchange(server, [recorded("tts_open"), *events], half_close=False)

    assert status is grpc.StatusCode.INVALID_ARGUMENT, what
    closed = got[-1].session_closed
    assert closed.state == pb.SESSION_STATE_FAILED
    assert closed.cause == recorded("refused_invalid_event").cause
    # The event before the refused one was accepted, where there is one.
    assert closed.last_accepted_sequence == (2 if len(events) > 1 else 0)


@pytest.mark.parametrize("text", ["", " \t\r\n\v\f"])
def test_a_segment_without_text_fails_the_session_as_empty_input(text: str) -> None:
    events = [recorded("tts_open"), _text(2, 1, text)]

    got, status = exchange(StreamServer(), events, half_close=False)

    assert status is grpc.StatusCode.INVALID_ARGUMENT
    closed = got[-1].session_closed
    assert (closed.state, closed.cause) == (pb.SESSION_STATE_FAILED, end_cause("empty_input"))
    assert closed.totals.input_text_bytes == 0


def _credits(got: list[Any]) -> list[tuple[int, int]]:
    """The bytes and the waiting segments each Accepted reports as used."""
    credits = [event.accepted.input_credit for event in got if event.HasField("accepted")]
    return [(credit.bytes.used, credit.waiting_segments.used) for credit in credits]


def test_a_third_waiting_segment_fails_the_session_as_recorded() -> None:
    server = StreamServer(hold_synthesis=True)
    events = [recorded("tts_open"), _text(2, 1), _text(3, 2), _text(4, 3)]

    got, status = exchange(server, events, half_close=False)

    assert status is grpc.StatusCode.RESOURCE_EXHAUSTED
    assert _credits(got) == [(18, 1), (36, 2)]
    _ends(got, "failed_input_credit_exceeded")
    assert got[-1].session_closed.last_accepted_sequence == 3


def test_a_started_segment_returns_its_credit_and_keeps_a_slot_of_its_own() -> None:
    server = StreamServer(hold_synthesis=True)
    opening = ([recorded("tts_open"), _text(2, 1), _text(3, 2)], [("start",)])

    got, status = converse(server, opening, ([_text(4, 3), _text(5, 4)], []), half_close=False)

    # Two wait beside the one started; a fourth is above the credit.
    assert status is grpc.StatusCode.RESOURCE_EXHAUSTED
    assert _credits(got) == [(18, 1), (36, 2), (18, 1), (36, 2)]
    assert got[-1].session_closed.last_accepted_sequence == 4


def test_text_above_the_byte_credit_fails_the_session() -> None:
    server = StreamServer(text_credit=600, hold_synthesis=True)
    events = [recorded("tts_open"), _text(2, 1, "x" * 400), _text(3, 2, "x" * 400)]

    got, status = exchange(server, events, half_close=False)

    assert status is grpc.StatusCode.RESOURCE_EXHAUSTED
    assert _credits(got) == [(400, 1)]
    _ends(got, "failed_input_credit_exceeded")


def test_text_before_the_answer_to_a_finalize_fails_the_session() -> None:
    server = StreamServer(hold_synthesis=True)
    events = [recorded("tts_open"), _text(2, 1), _finalize(3), _text(4, 2)]

    got, status = exchange(server, events, half_close=False)

    assert status is grpc.StatusCode.FAILED_PRECONDITION
    closed = got[-1].session_closed
    assert (closed.state, closed.cause) == (
        pb.SESSION_STATE_FAILED,
        end_cause("illegal_transition"),
    )
    assert closed.last_accepted_sequence == 2
    assert "synthesis_completed" not in _bodies(got)


def test_a_finalize_is_answered_after_its_segments_with_the_counts_since_the_last() -> None:
    # 400 bytes twice is above the 700 allowed between two Finalizes, and the Finalize between
    # them starts the count again.
    server = StreamServer(max_synthesis_text_bytes=700, hold_synthesis=True)
    sound = [("start",), ("speak", 960), ("complete",)]
    first = ([recorded("tts_open"), _text(2, 1, "x" * 400), _finalize(3), _finalize(4)], sound)
    second = ([_text(5, 2, "x" * 400), _finalize(6)], [("start",), ("speak", 100), ("complete",)])

    got, status = converse(server, first, second, ([_finalize(7)], []))

    assert status is grpc.StatusCode.OK
    answers = [event.synthesis_completed for event in got if event.HasField("synthesis_completed")]
    assert [(a.segment_count, a.total_samples, a.finalize_sequence) for a in answers] == [
        (1, 960, 3),
        (1, 100, 6),
        (0, 0, 7),
    ]
    task = [body for body in _bodies(got) if body not in ("ready", "accepted")]
    assert task == [
        *["audio_chunk", "audio_chunk", "segment_completed", "synthesis_completed"],
        *["audio_chunk", "segment_completed", "synthesis_completed"],
        *["synthesis_completed", "session_closed"],
    ]
    totals = got[-1].session_closed.totals
    assert (totals.input_text_bytes, totals.output_audio_samples, totals.segments_completed) == (
        800,
        1060,
        2,
    )


def test_a_half_close_completes_every_accepted_segment_and_answers_no_finalize() -> None:
    server = StreamServer(segment_samples=1000)

    got, status = exchange(server, [recorded("tts_open"), _text(2, 1), _text(3, 2)])

    assert status is grpc.StatusCode.OK
    chunks = [event.audio_chunk for event in got if event.HasField("audio_chunk")]
    # 20 ms is 480 samples: only a segment's last chunk is shorter.
    assert [
        (c.segment_id, c.chunk_index, c.segment_sample_offset, len(c.data)) for c in chunks
    ] == [
        (1, 0, 0, 960),
        (1, 1, 480, 960),
        (1, 2, 960, 80),
        (2, 0, 0, 960),
        (2, 1, 480, 960),
        (2, 2, 960, 80),
    ]
    assert all(chunk.format == got[0].ready.output_format for chunk in chunks)
    done = [event.segment_completed for event in got if event.HasField("segment_completed")]
    assert [(d.segment_id, d.total_samples, d.chunk_count) for d in done] == [
        (1, 1000, 3),
        (2, 1000, 3),
    ]
    assert "synthesis_completed" not in _bodies(got)
    task = [body for body in _bodies(got) if body not in ("ready", "accepted")]
    assert task == [*[*["audio_chunk"] * 3, "segment_completed"] * 2, "session_closed"]
    assert got[-1].session_closed.totals.segments_completed == 2


def test_a_half_close_waits_for_a_segment_that_has_not_completed() -> None:
    server = StreamServer(hold_synthesis=True)
    sound = [("start",), ("speak", 480), ("complete",)]

    got, status = converse(server, ([recorded("tts_open"), _text(2, 1)], sound))

    assert status is grpc.StatusCode.OK
    assert _bodies(got)[-3:] == ["audio_chunk", "segment_completed", "session_closed"]
    assert got[-1].session_closed.totals.segments_completed == 1


def test_a_cancel_stops_a_segments_audio_where_it_is() -> None:
    server = StreamServer(hold_synthesis=True)
    cancel = recorded("cancel")
    cancel.sequence = 3
    opening = ([recorded("tts_open"), _text(2, 1)], [("start",), ("speak", 480)])

    got, status = converse(server, opening, ([cancel], []), half_close=False)

    assert status is grpc.StatusCode.CANCELLED
    assert _bodies(got)[-3:] == ["audio_chunk", "cancel_accepted", "session_closed"]
    _ends(got, "closed_client_cancelled")
    totals = got[-1].session_closed.totals
    assert (totals.output_audio_samples, totals.segments_completed) == (480, 0)


def test_an_end_the_server_decides_stops_its_own_synthesis_between_two_chunks() -> None:
    # Fifty seconds of audio: the end is taken in turn with the chunks, not after them.
    server = StreamServer(segment_samples=1_200_000)
    opening = ([recorded("tts_open"), _text(2, 1)], [("end", "worker_shutdown")])

    got, status = converse(server, opening, half_close=False)

    assert status is grpc.StatusCode.UNAVAILABLE
    assert _bodies(got).count("audio_chunk") < 2500
    assert "segment_completed" not in _bodies(got)
