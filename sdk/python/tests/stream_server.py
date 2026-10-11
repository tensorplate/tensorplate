"""In-process stream servers for the streaming client's tests.

``StreamServer`` serves speech-to-text and text-to-speech sessions as the
stream schema and ``docs/architecture/serving-worker.md`` ("Stream
listener", "Logical sessions", "Session dispatch") say a worker does, and
refuses what they say it refuses: input before its Open, a second Open,
input of the other mode, input above the credit, and input while a Finalize
is unanswered; audio above the frame bound or after a short frame; a text
segment that is empty, above a text bound, or whose id is not above every
earlier one. It keeps the envelope itself: sequences, credit, utterance ids,
sample offsets, chunk indexes, the Finalize an answer names, the totals. It
has no recognizer, no synthesizer and no timer, so a test supplies what
those would: the text of a transcript, where an automatic endpoint falls
(``endpoint()``), when audio is consumed (``consume()``), when a segment
starts and how much audio it becomes (``start()``, ``speak()``,
``complete()``, or `segment_samples`), and a timer or the worker ending the
session (``end()``). A `fault` makes it break one rule, for the tests of
what the client does then. ``test_streaming_server.py`` holds it to the
recorded frames under ``test/contract/fixtures/stream/v1``.

``Playback`` serves one call from a recording instead: the recorded server
frames, byte for byte or with a test's `changes`, each run after the client
frames that precede it.
"""

from __future__ import annotations

import asyncio
import contextlib
import re
from collections import deque
from collections.abc import AsyncIterator, Awaitable, Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import grpc
import grpc.aio

from tensorplate.stream.v1 import session_pb2 as pb

REPOSITORY = Path(__file__).resolve().parents[3]
FRAMES = REPOSITORY / "test/contract/fixtures/stream/v1"
_REASONS = REPOSITORY / "docs/observability/failure-reasons.md"
_SERVICE = "tensorplate.stream.v1.SessionService"

Code = grpc.StatusCode
# The status a reason ends a call with, by the kind of reason: invalid input, a
# precondition, a bound, a timer, unavailability, the client's own cancel.
STATUS = {
    "invalid_event": Code.INVALID_ARGUMENT,
    "empty_input": Code.INVALID_ARGUMENT,
    "backend_unsupported_capability": Code.FAILED_PRECONDITION,
    "target_unresolved": Code.FAILED_PRECONDITION,
    "target_mismatch": Code.FAILED_PRECONDITION,
    "stale_generation": Code.FAILED_PRECONDITION,
    "illegal_transition": Code.FAILED_PRECONDITION,
    "session_count_limit": Code.RESOURCE_EXHAUSTED,
    "input_credit_exceeded": Code.RESOURCE_EXHAUSTED,
    "slow_consumer": Code.RESOURCE_EXHAUSTED,
    "idle_timeout": Code.DEADLINE_EXCEEDED,
    "admission_closed": Code.UNAVAILABLE,
    "worker_shutdown": Code.UNAVAILABLE,
    "backend_reset": Code.UNAVAILABLE,
    "client_cancelled": Code.CANCELLED,
}
# Reasons that end a session failed; every other one ends it closed (serving-worker.md,
# "How sessions end").
FAILS = (
    "invalid_event",
    "empty_input",
    "input_credit_exceeded",
    "backend_reset",
    "illegal_transition",
)
_SAMPLE_BYTES = {pb.AUDIO_ENCODING_PCM_S16LE: 2, pb.AUDIO_ENCODING_MULAW: 1}
_SHORT_TEXT = r"[ -~]{%d,64}"
# The audio of every synthetic segment: the recording's two samples, repeated.
_TONE = b"\x00\x10\x00\xf0"


def frames(file: str = "*") -> list[tuple[str, str, bytes]]:
    """The recorded frames of `file` in its order: name, message type, bytes."""
    rows = []
    for path in sorted(FRAMES.glob(f"{file}.frames")):
        for line in path.read_text(encoding="utf-8").splitlines():
            if line and not line.startswith("#"):
                name, kind, encoded = line.split("\t")[:3]
                rows.append((name, kind.rsplit(".", 1)[1], bytes.fromhex(encoded)))
    return rows


def recorded(name: str) -> Any:
    """The recorded frame called `name`, decoded with the SDK's generated modules."""
    for frame, kind, encoded in frames():
        if frame == name:
            return getattr(pb, kind).FromString(encoded)
    raise AssertionError(f"no recorded frame {name}")


def end_cause(reason: str) -> Any:
    """`reason` with the error code the failure reason taxonomy pairs with it."""
    row = re.search(rf"^\| `{reason}`.*\| `(\w+)`\s*\|$", _REASONS.read_text("utf-8"), re.M)
    assert row, f"{reason} is not in the failure reason taxonomy"
    return pb.EndCause(
        reason=pb.FailureReason.Value(f"FAILURE_REASON_{reason.upper()}"),
        code=pb.ErrorCode.Value(f"ERROR_CODE_{row.group(1).upper()}"),
    )


def _wire(event: Any) -> bytes:
    """A server event as sent; bytes go out as they are, which is how a recording does."""
    return event if isinstance(event, bytes) else bytes(event.SerializeToString())


@contextlib.asynccontextmanager
async def serve(session: Callable[[Any, Any], Awaitable[None]]) -> AsyncIterator[str]:
    """Serve `session` as the schema's one method on a loopback port."""
    # A fixed HTTP/2 window, so that a server that reads nothing holds a writer back.
    server = grpc.aio.server(options=[("grpc.http2.bdp_probe", 0)])
    method = grpc.stream_stream_rpc_method_handler(
        session, request_deserializer=pb.ClientEvent.FromString, response_serializer=_wire
    )
    handler = grpc.method_handlers_generic_handler(_SERVICE, {"Session": method})
    server.add_generic_rpc_handlers((handler,))
    port = server.add_insecure_port("127.0.0.1:0")
    await server.start()
    try:
        yield f"127.0.0.1:{port}"
    finally:
        await server.stop(None)


@dataclass
class Playback:
    """One call served from the recording `file`, and the client frames it was sent.

    `changes` names recorded server frames and what a test does to each
    before it is sent: a function that alters the decoded event, or ``None``
    for a frame the server leaves out, which the later sequences close over.
    """

    file: str
    changes: dict[str, Callable[[Any], None] | None] = field(default_factory=dict)
    received: list[Any] = field(default_factory=list)

    def serving(self) -> contextlib.AbstractAsyncContextManager[str]:
        return serve(self.Session)

    async def Session(self, unused_requests: Any, context: Any) -> None:
        # Each run of server frames, with the number of client frames recorded before it.
        runs: list[tuple[int, list[bytes]]] = []
        sent = left_out = 0
        for name, kind, encoded in frames(self.file):
            if kind == "ClientEvent":
                sent += 1
                continue
            if name in self.changes or left_out:
                event = pb.ServerEvent.FromString(encoded)
                change = self.changes.get(name, lambda _: None)
                if change is None:
                    left_out += 1
                    continue
                event.sequence -= left_out
                change(event)
                encoded = event.SerializeToString()
            if runs and runs[-1][0] == sent:
                runs[-1][1].append(encoded)
            else:
                runs.append((sent, [encoded]))
        for count, replies in runs:
            while len(self.received) < count:
                self.received.append(await context.read())
            if replies is runs[-1][1]:
                # The recording's last run answers the half-close, which is not a frame.
                assert await context.read() is grpc.aio.EOF
            for reply in replies:
                await context.write(reply)


class _Ends(Exception):
    """The session ends with the reason given."""


@dataclass
class StreamServer:
    """One worker's stream listener: what it serves, what it was sent, and its faults."""

    deployment_id: str = "synthetic-deployment"
    generation: int = 7
    descriptor_digest: str = "sha256:" + "0" * 64
    languages: tuple[str, ...] = ("en",)
    formats: tuple[tuple[int, int, int], ...] = (
        (pb.AUDIO_ENCODING_MULAW, 8000, 1),
        (pb.AUDIO_ENCODING_PCM_S16LE, 16000, 1),
    )
    min_frame_ms: int = 20
    max_frame_ms: int = 40
    voices: tuple[str, ...] = ("synthetic_voice",)
    output_format: tuple[Any, int, int] = (pb.AUDIO_ENCODING_PCM_S16LE, 24000, 1)
    max_segment_text_bytes: int = 512
    max_synthesis_text_bytes: int = 4096
    text_credit: int = 8192
    # Samples a segment becomes when the server synthesizes by itself.
    segment_samples: int = 960
    # A segment starts, sounds and completes only at `start()`, `speak()` and `complete()`.
    hold_synthesis: bool = False
    heartbeat_interval_ms: int = 10000
    output_stall_timeout_ms: int = 5000
    max_sessions: int = 1
    admission_open: bool = True
    # Accepted audio stays charged until `consume()`.
    hold_credit: bool = False
    # No Accepted is sent until `accept()`.
    hold_accepted: bool = False
    # A Finalize and a half-close stay unanswered until `endpoint()` and `final()`.
    hold_finals: bool = False
    # A partial transcript follows the first frame of each utterance.
    partials: bool = False
    # A partial transcript of this many bytes follows every frame.
    partial_bytes: int = 0
    fault: str = ""
    calls: int = 0
    live: int = 0
    # Client events read after the Open, which a session whose write is held back has not taken.
    arrived: int = 0
    half_closed: bool = False
    received: list[Any] = field(default_factory=list)
    refusals: list[Any] = field(default_factory=list)
    closed: list[Any] = field(default_factory=list)
    _session: _Live | None = None
    _changed: asyncio.Event = field(default_factory=asyncio.Event)

    def serving(self) -> contextlib.AbstractAsyncContextManager[str]:
        return serve(self.Session)

    def bodies(self) -> list[str | None]:
        return [event.WhichOneof("body") for event in self.received]

    async def until(self, done: Callable[[], bool]) -> None:
        """Wait for `done`, asked again after each thing the server does."""
        while not done():
            await self._changed.wait()

    def _notify(self) -> None:
        self._changed.set()
        self._changed = asyncio.Event()

    def _tell(self, step: str, *arguments: Any) -> None:
        """Queue a step for the live session, which takes it in turn with the client's events."""
        assert self._session is not None, "no session is live"
        self._session.inbox.put_nowait((step, arguments))

    def accept(self) -> None:
        """Send an Accepted: the last input accepted and the credit as it stands."""
        self._tell("accepted")

    def consume(self, size: int | None = None) -> None:
        """Take `size` bytes of the audio the session holds, or all, which returns their credit."""
        self._tell("consume", size)

    def partial(self, text: str) -> None:
        """Send the next revision of the open utterance's partial transcript."""
        self._tell("partial", text)

    def endpoint(self, at: int | None = None) -> None:
        """End the open utterance at sample `at`, or at the last sample received.

        The endpoint is the Finalize's when one waits for it, the
        half-close's when the session drains, and automatic otherwise.
        """
        self._tell("endpoint", at)

    def final(self, segments: list[tuple[int, int, str]] | None = None) -> None:
        """Send the final transcript owed longest, after its endpoint if that is still due.

        `segments` are (start_us, end_us, text); none given is one synthetic
        segment over the utterance, or no segment for an utterance without audio.
        """
        self._tell("final", segments)

    def start(self) -> None:
        """Start the segment that waited longest, which returns its input credit."""
        self._tell("start")

    def speak(self, samples: int) -> None:
        """Send `samples` more of the started segment's audio, in chunks of 20 ms."""
        self._tell("speak", samples)

    def complete(self) -> None:
        """Complete the started segment."""
        self._tell("complete")

    def end(self, reason: str) -> None:
        """End the live session from the server's side."""
        self._tell("end", reason)

    def hear(self) -> None:
        """Let a `deaf` server start reading.

        Until then it takes only the steps a test queues, so `end()` ends
        a session while the transport still holds what the client wrote.
        """
        assert self._session is not None, "no session is live"
        self._session.hearing.set()

    def _admit(self, first: Any) -> Any:
        """The Ready for an Open, or the reason it is refused with."""
        opened = first.open
        if first.WhichOneof("body") != "open" or first.sequence != 1 or first.session_id:
            raise _Ends("invalid_event")
        if opened.WhichOneof("target") == "selector":
            raise _Ends("target_unresolved")
        target = opened.resolved
        named = re.fullmatch(r"[A-Za-z0-9._-]{1,128}", target.deployment_id)
        if not named or target.deployment_id in (".", ".."):
            raise _Ends("invalid_event")
        if not re.fullmatch(_SHORT_TEXT % 1, opened.language):
            raise _Ends("invalid_event")
        speaks = opened.mode == pb.SESSION_MODE_TTS_STREAMING
        # A text-to-speech Open names a voice and no input format.
        if speaks and (
            opened.HasField("input_format") or not re.fullmatch(_SHORT_TEXT % 1, opened.voice)
        ):
            raise _Ends("invalid_event")
        served = (self.deployment_id, self.descriptor_digest)
        if (target.deployment_id, target.descriptor_digest) != served:
            raise _Ends("target_mismatch")
        if target.generation != self.generation:
            raise _Ends("stale_generation")
        audio = opened.input_format
        declared = (audio.encoding, audio.sample_rate_hz, audio.channels) in self.formats
        if speaks:
            declared = opened.voice in self.voices
            encoding, rate, channels = self.output_format
            audio = pb.AudioFormat(encoding=encoding, sample_rate_hz=rate, channels=channels)
        if (
            opened.mode not in (pb.SESSION_MODE_STT_STREAMING, pb.SESSION_MODE_TTS_STREAMING)
            or opened.language not in self.languages
            or not declared
        ):
            raise _Ends("backend_unsupported_capability")
        if not self.admission_open:
            raise _Ends("admission_closed")
        if self.live >= self.max_sessions:
            raise _Ends("session_count_limit")
        second = audio.sample_rate_hz * audio.channels * _SAMPLE_BYTES[audio.encoding]
        ready = pb.Ready(
            session_id=f"synthetic-session-{self.calls}",
            generation=self.generation,
            descriptor_digest=self.descriptor_digest,
            limits=pb.SessionLimits(
                idle_timeout_ms=60000,
                heartbeat_interval_ms=self.heartbeat_interval_ms,
                liveness_timeout_ms=30000,
                max_duration_ms=3600000,
                finalize_deadline_ms=120000 if speaks else 10000,
                output_stall_timeout_ms=self.output_stall_timeout_ms,
            ),
            remaining_duration_ms=3600000,
            status=pb.SessionStatus(
                state=pb.SESSION_STATE_ACTIVE,
                input_credit=pb.InputCredit(bytes=pb.Usage(limit=second)),
                output_metadata_bytes=pb.Usage(limit=16384),
            ),
        )
        if not speaks:
            ready.input_format.CopyFrom(audio)
            ready.limits.speech_to_text.CopyFrom(
                pb.SpeechToTextLimits(
                    min_frame_ms=self.min_frame_ms,
                    max_frame_ms=self.max_frame_ms,
                    max_utterance_ms=30000,
                )
            )
            return ready
        ready.output_format.CopyFrom(audio)
        ready.limits.text_to_speech.CopyFrom(
            pb.TextToSpeechLimits(
                max_segment_text_bytes=self.max_segment_text_bytes,
                max_segment_audio_ms=10000,
                max_synthesis_text_bytes=self.max_synthesis_text_bytes,
                max_synthesis_audio_ms=120000,
            )
        )
        # Two segments may wait beside the one being synthesized or delivered; two seconds
        # of audio may wait undelivered.
        credit = pb.InputCredit(
            bytes=pb.Usage(limit=self.text_credit), waiting_segments=pb.Usage(limit=2)
        )
        ready.status.input_credit.CopyFrom(credit)
        ready.status.output_pcm_bytes.limit = 2 * second
        return ready

    async def Session(self, unused_requests: Any, context: Any) -> None:
        self.calls += 1
        try:
            first = await context.read()
            if first is grpc.aio.EOF:
                return
            self.received.append(first)
            ready = self._admit(first)
        except _Ends as refusal:
            reason = str(refusal)
            refused = pb.OpenRefused(cause=end_cause(reason), admission=pb.ADMISSION_NOT_ADMITTED)
            self.refusals.append(refused)
            await context.abort(STATUS[reason], reason)
        self.live += 1
        try:
            await _Live(self, context, ready).run()
        finally:
            self.live -= 1
            self._notify()


class _Live:
    """One admitted session. Each step is a client event or one a test queued, taken in turn."""

    def __init__(self, server: StreamServer, context: Any, ready: Any) -> None:
        self.server, self.context, self.ready = server, context, ready
        self.speaks = ready.HasField("output_format")
        audio = ready.output_format if self.speaks else ready.input_format
        self.rate, self.width = audio.sample_rate_hz, _SAMPLE_BYTES[audio.encoding] * audio.channels
        self.limit = ready.status.input_credit.bytes.limit
        # Accepted segments whose synthesis has not started: id and bytes of text.
        self.waiting: deque[tuple[int, int]] = deque()
        # The started segment, the samples and chunks sent of it, and the samples still due.
        self.segment: int | None = None
        self.spoken = self.chunks = self.due = 0
        self.last_segment = -1
        # Text accepted since the last Finalize; segments and samples completed since its answer.
        self.synthesis_text = self.completed = self.completed_samples = 0
        self.text_bytes = self.audio_samples = self.segments = 0
        self.stepping = False
        self.out = self.used = self.samples = self.last = self.finals = 0
        self.expected = 1
        self.utterance, self.utterance_start, self.revision = 1, 0, 0
        # Ended utterances whose final transcript is owed: id, first sample, end, Finalize.
        self.owed: deque[tuple[int, int, int, int]] = deque()
        # The client Finalize not answered yet; audio is refused until it is.
        self.finalize = 0
        self.short = self.draining = self.done = False
        self.hearing = asyncio.Event()
        self.inbox: asyncio.Queue[tuple[str, tuple[Any, ...]]] = asyncio.Queue()
        server._session = self

    async def emit(self, **body: Any) -> None:
        self.out += 1
        await self.context.write(pb.ServerEvent(sequence=self.out, **body))

    async def accepted(self) -> None:
        credit = pb.InputCredit(bytes=pb.Usage(used=self.used, limit=self.limit))
        if self.speaks:
            room = self.ready.status.input_credit.waiting_segments.limit
            credit.waiting_segments.CopyFrom(pb.Usage(used=len(self.waiting), limit=room))
        await self.emit(accepted=pb.Accepted(accepted_sequence=self.last, input_credit=credit))

    async def pump(self) -> None:
        if self.server.fault.startswith("deaf"):
            await self.hearing.wait()
        while (event := await self.context.read()) is not grpc.aio.EOF:
            self.inbox.put_nowait(("event", (event,)))
            self.server.arrived += 1
            self.server._notify()
        self.inbox.put_nowait(("half_close", ()))

    async def run(self) -> None:
        fault = self.server.fault
        if fault == "silent":
            await asyncio.Event().wait()
        if fault == "event_before_ready":
            await self.emit(pong=pb.Pong())
        if fault == "sequence_gap":
            self.out += 1
        if fault == "other_generation":
            self.ready.generation += 1
        if fault == "no_credit":
            self.ready.status.ClearField("input_credit")
        await self.emit(ready=self.ready)
        if fault == "deaf_undecodable":
            await self.hearing.wait()
        if fault == "drop":
            await self.context.abort(Code.UNAVAILABLE, "synthetic connection loss")
        if fault.endswith("undecodable"):
            await self.context.write(b"\xff")
        strangers = {
            "unknown_event": "added_server_body",
            "audio_chunk": "tts_chunk_1_1",
            "transcript": "stt_partial_1",
        }
        if fault in strangers:
            self.out += 1
            strange = recorded(strangers[fault])
            strange.sequence = self.out
            await self.context.write(strange)
        self.server._notify()
        pump = asyncio.create_task(self.pump())
        try:
            while not self.done:
                step, arguments = await self.inbox.get()
                await getattr(self, step)(*arguments)
                await self.settle()
                self.server._notify()
        except _Ends as end:
            await self.close(str(end))
        finally:
            pump.cancel()

    async def settle(self) -> None:
        """Send what the session owes unless a test holds it back, and close a drained session."""
        if self.speaks:
            return await self.voice()
        ends = self.finalize and not any(self.finalize == entry[3] for entry in self.owed)
        if not self.server.hold_finals:
            # A final transcript owed to an earlier endpoint comes before the Finalize's own.
            while self.owed:
                await self.final()
            if ends or (self.draining and self.samples > self.utterance_start):
                await self.final()
        idle = not self.owed and not self.finalize and self.samples == self.utterance_start
        if self.draining and idle and not self.done:
            await self.close(None)

    async def close(self, reason: str | None) -> None:
        self.done = True
        if reason is None and self.server.fault == "ok_without_closed":
            return
        closed = pb.SessionClosed(
            state=pb.SESSION_STATE_FAILED if reason in FAILS else pb.SESSION_STATE_CLOSED,
            admission=pb.ADMISSION_ADMITTED,
            last_accepted_sequence=self.last,
            totals=pb.SessionTotals(
                input_audio_samples=self.samples,
                input_text_bytes=self.text_bytes,
                output_audio_samples=self.audio_samples,
                utterances_completed=self.finals,
                segments_completed=self.segments,
            ),
        )
        if reason is not None:
            closed.cause.CopyFrom(end_cause(reason))
        self.server.closed.append(closed)
        await self.emit(session_closed=closed)
        if self.server.fault == "deaf_status_withheld":
            # gRPC gives a client its call's status after the messages, and this one stays unread.
            await self.emit(pong=pb.Pong())
        if reason is not None and self.server.fault != "ok_with_cause":
            await self.context.abort(STATUS[reason], reason)

    async def half_close(self) -> None:
        self.draining = self.server.half_closed = True

    async def end(self, reason: str) -> None:
        raise _Ends(reason)

    async def event(self, event: Any) -> None:
        self.server.received.append(event)
        self.expected += 1
        body = event.WhichOneof("body")
        # Only a Cancel sent before the client read Ready may name no session.
        early = body == "cancel" and not event.session_id
        if event.sequence != self.expected or not (
            early or event.session_id == self.ready.session_id
        ):
            raise _Ends("invalid_event")
        if body == "ping":
            await self.emit(pong=pb.Pong())
        elif body == "status_request":
            await self.emit(status=pb.SessionStatus(state=pb.SESSION_STATE_ACTIVE))
        elif body == "cancel":
            await self.emit(cancel_accepted=pb.CancelAccepted())
            raise _Ends("client_cancelled")
        elif body == "finalize":
            # Repeated before its answer, a Finalize starts nothing and has no answer of its own.
            # Only the client's own Finalize or half-close makes a short frame the last.
            if not self.finalize:
                self.finalize, self.short, self.synthesis_text = event.sequence, False, 0
        elif body == ("text_segment" if self.speaks else "audio"):
            await (self.text if self.speaks else self.audio)(event)
        else:
            # A second Open, a body the schema lacks, or input of another mode.
            raise _Ends("invalid_event")

    async def audio(self, event: Any) -> None:
        audio, limits = event.audio, self.ready.limits.speech_to_text
        count = audio.sample_count
        if self.finalize:
            raise _Ends("illegal_transition")
        if not audio.data:
            raise _Ends("empty_input")
        if (
            count * self.width != len(audio.data)
            or audio.sample_offset != self.samples
            or count * 1000 > limits.max_frame_ms * self.rate
            or self.short
        ):
            raise _Ends("invalid_event")
        if self.used + len(audio.data) > self.limit:
            raise _Ends("input_credit_exceeded")
        self.short = count * 1000 < limits.min_frame_ms * self.rate
        self.samples, self.last = self.samples + count, event.sequence
        self.used += len(audio.data)
        await self.consume(0 if self.server.hold_credit else None)
        if self.server.partials and self.samples - count == self.utterance_start:
            await self.partial("synthetic")
        if self.server.partial_bytes:
            await self.partial("x" * self.server.partial_bytes)

    async def consume(self, size: int | None) -> None:
        self.used -= self.used if size is None else size
        if not self.server.hold_accepted:
            await self.accepted()

    async def partial(self, text: str) -> None:
        self.revision += 1
        partial = pb.PartialTranscript(
            utterance_id=self.utterance, revision=self.revision, text=text
        )
        await self.emit(partial_transcript=partial)

    async def endpoint(self, at: int | None = None) -> None:
        end = self.samples if at is None else at
        sequence, reason = 0, pb.ENDPOINT_REASON_SILENCE
        if self.finalize and not any(self.finalize == entry[3] for entry in self.owed):
            sequence, reason = self.finalize, pb.ENDPOINT_REASON_CLIENT_FINALIZE
        elif self.draining:
            reason = pb.ENDPOINT_REASON_HALF_CLOSE
        self.owed.append((self.utterance, self.utterance_start, end, sequence))
        endpoint = pb.EndpointDetected(
            utterance_id=self.utterance, end_sample_offset=end, reason=reason
        )
        for _ in range(
            {"final_without_endpoint": 0, "second_endpoint": 2}.get(self.server.fault, 1)
        ):
            await self.emit(endpoint_detected=endpoint)
        self.utterance, self.utterance_start, self.revision = self.utterance + 1, end, 0

    async def final(self, segments: list[tuple[int, int, str]] | None = None) -> None:
        fault = self.server.fault
        if not self.owed:
            await self.endpoint()
        utterance, start, end, sequence = self.owed.popleft()
        if segments is None:
            whole = (start * 1_000_000 // self.rate, -(-end * 1_000_000 // self.rate))
            segments = [(*whole, "synthetic transcript")] if end > start else []
        final = pb.FinalTranscript(
            utterance_id=utterance,
            segments=[
                pb.TranscriptSegment(start_us=a, end_us=b, text=text) for a, b, text in segments
            ],
            end_sample_offset=end,
            finalize_sequence=sequence + (100 if fault == "other_finalize" else 0),
        )
        if fault == "partial_after_endpoint":
            late = pb.PartialTranscript(utterance_id=utterance, revision=2, text="late")
            await self.emit(partial_transcript=late)
        await self.emit(final_transcript=final)
        self.finals += 1
        if sequence:
            self.finalize = 0

    async def text(self, event: Any) -> None:
        segment, limits = event.text_segment, self.ready.limits.text_to_speech
        size = len(segment.text.encode())
        if self.finalize:
            raise _Ends("illegal_transition")
        if not segment.text.strip(" \t\n\v\f\r"):
            raise _Ends("empty_input")
        if (
            segment.segment_id <= self.last_segment
            or size > limits.max_segment_text_bytes
            or self.synthesis_text + size > limits.max_synthesis_text_bytes
            or not re.fullmatch(_SHORT_TEXT % 0, segment.turn_id)
        ):
            raise _Ends("invalid_event")
        room = self.ready.status.input_credit.waiting_segments.limit
        if self.used + size > self.limit or len(self.waiting) == room:
            raise _Ends("input_credit_exceeded")
        self.waiting.append((segment.segment_id, size))
        self.last_segment, self.last = segment.segment_id, event.sequence
        self.used, self.text_bytes = self.used + size, self.text_bytes + size
        self.synthesis_text += size
        if not self.server.hold_accepted:
            await self.accepted()

    async def start(self) -> None:
        assert self.segment is None, "a segment is started while another holds the slot"
        self.segment, size = self.waiting.popleft()
        # Its bytes and its waiting place return now; the slot it takes returns when it completes.
        self.used -= size
        self.spoken = self.chunks = 0
        self.due = 0 if self.server.hold_synthesis else self.server.segment_samples
        if not self.server.hold_accepted:
            await self.accepted()

    async def speak(self, samples: int) -> None:
        full = self.rate * 20 // 1000
        while samples:
            count = min(samples, full)
            first = self.spoken * self.width % len(_TONE)
            chunk = pb.AudioChunk(
                segment_id=self.segment,
                chunk_index=self.chunks,
                segment_sample_offset=self.spoken,
                format=self.ready.output_format,
                data=(_TONE * (count * self.width))[first : first + count * self.width],
            )
            await self.emit(audio_chunk=chunk)
            samples -= count
            self.spoken += count
            self.chunks += 1
            self.audio_samples += count

    async def complete(self) -> None:
        done = pb.SegmentCompleted(
            segment_id=self.segment, total_samples=self.spoken, chunk_count=self.chunks
        )
        await self.emit(segment_completed=done)
        self.segment = None
        self.completed += 1
        self.completed_samples += self.spoken
        self.segments += 1

    async def more(self) -> None:
        self.stepping = False

    async def voice(self) -> None:
        """Do the next thing a text session owes, one event at a time.

        The rest is queued behind whatever arrived meanwhile, so a Cancel or
        an end stops the output where it is.
        """
        held = self.server.hold_synthesis
        if self.segment is not None and not held:
            if self.due:
                count = min(self.due, self.rate * 20 // 1000)
                await self.speak(count)
                self.due -= count
            else:
                await self.complete()
        elif self.segment is None and self.waiting and not held:
            await self.start()
        elif self.segment is None and not self.waiting and self.finalize:
            answer = pb.SynthesisCompleted(
                segment_count=self.completed,
                total_samples=self.completed_samples,
                finalize_sequence=self.finalize,
            )
            await self.emit(synthesis_completed=answer)
            self.finalize = self.completed = self.completed_samples = 0
        elif self.segment is None and not self.waiting and self.draining and not self.done:
            await self.close(None)
        else:
            return
        if not self.stepping and not self.done:
            self.stepping = True
            self.inbox.put_nowait(("more", ()))
