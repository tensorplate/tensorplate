"""Text-to-speech sessions: text segments in under input credit, audio out through a buffer."""

from __future__ import annotations

import enum
from collections import deque
from collections.abc import Callable
from typing import Any

from tensorplate.speech._session import Session, Targets, enum_name
from tensorplate.speech.events import (
    AudioChunk,
    AudioFormat,
    SegmentCompleted,
    SynthesisCompleted,
    Target,
)
from tensorplate.stream.v1 import session_pb2 as pb

TextToSpeechEvent = AudioChunk | SegmentCompleted | SynthesisCompleted

# Audio a session holds for its caller before it stops reading: as much as the server may
# itself hold undelivered.
PLAYBACK_BUFFER_MS = 2000
CHUNK_MS = 20
# What a close is given beyond the server's finalize deadline when its caller named no timeout.
CLOSE_MARGIN_S = 30.0


class Follow(enum.Enum):
    """What ``SpeechClient.synthesize()`` takes as its `close_timeout` in place of seconds.

    ``FINALIZE_DEADLINE``, the default, follows a limit the server declares:
    the ``limits.finalize_deadline_ms`` of the session's ``Ready`` and 30
    seconds more. No other timeout takes it.
    """

    FINALIZE_DEADLINE = "limits.finalize_deadline_ms and a margin"

    def __repr__(self) -> str:
        return f"{type(self).__name__}.{self.name}"


class TextToSpeechSession(Session[TextToSpeechEvent]):
    """One text-to-speech session, from ``SpeechClient.synthesize``.

    Iterating yields each segment's ``AudioChunk`` events and then its
    ``SegmentCompleted``, segment after segment in the order sent, and a
    ``SynthesisCompleted`` for each ``finalize()``. Read them while the
    session sends, in a task of their own.

    Audio not taken is held in a playback buffer of two seconds of the
    output format, counted in bytes of audio; other events are held up to
    16 KiB. Past either bound the session reads nothing more from the
    server until something is taken. The server then stops synthesizing,
    since it holds no more than two seconds of undelivered audio itself,
    and the credit ``send_text()`` waits for stops returning. A caller that
    plays the audio as it reads it keeps the session moving at the pace of
    playback.
    """

    _sends_segments = True

    @classmethod
    def _create(
        cls,
        connect: Callable[[], Any],
        target: Target,
        *,
        language: str,
        voice: str,
        trace_id: str,
        turn_id: str,
        open_timeout: float | None,
        close_timeout: float | Follow | None,
        cancel_timeout: float,
        targets: Targets | None = None,
    ) -> TextToSpeechSession:
        session = cls.__new__(cls)
        follows = close_timeout is Follow.FINALIZE_DEADLINE
        # A close needs an open session, and Ready sets the bound that follows it.
        given = None if close_timeout is Follow.FINALIZE_DEADLINE else close_timeout
        session._setup(connect, target, open_timeout, given, cancel_timeout, targets)
        session._configure(language, voice, trace_id, turn_id, follows)
        return session

    def _configure(
        self, language: str, voice: str, trace_id: str, turn_id: str, close_follows_deadline: bool
    ) -> None:
        self._close_follows_deadline = close_follows_deadline
        self._language, self._voice = language, voice
        self._trace_id, self._turn_id = trace_id, turn_id
        self._format: AudioFormat | None = None
        self._next_segment = 1
        # Bytes of text sent since the last Finalize.
        self._synthesis_text = 0
        # Segments sent whose completion was not read, oldest first, and what was read of
        # the oldest: its chunks, its samples, and whether the last chunk was a short one.
        self._segments: deque[int] = deque()
        self._chunks = self._samples = 0
        self._short = False
        # Segments completed and their samples: since the last SynthesisCompleted, and in all.
        self._synthesized = self._synthesized_samples = 0
        self._completed = self._heard = 0

    @property
    def output_format(self) -> AudioFormat:
        """The format of every ``AudioChunk``, which the server chose for the voice."""
        self._opened()
        assert self._format is not None
        return self._format

    def _open_body(self, resolved: pb.ResolvedTarget) -> pb.Open:
        return pb.Open(
            resolved=resolved,
            mode=pb.SESSION_MODE_TTS_STREAMING,
            language=self._language,
            voice=self._voice,
            trace_id=self._trace_id,
            turn_id=self._turn_id,
        )

    def _check_ready(self, ready: pb.Ready) -> None:
        limits = ready.limits.text_to_speech
        if not (limits.max_segment_text_bytes and limits.max_synthesis_text_bytes):
            raise self._violation("Ready declares no text bounds")
        wire = ready.output_format
        audio = AudioFormat(
            enum_name(pb.AudioEncoding, wire.encoding), wire.sample_rate_hz, wire.channels
        )
        try:
            second = audio.sample_width * audio.sample_rate_hz
        except ValueError:
            second = 0
        if not second:
            raise self._violation("Ready declares no output format this client knows")
        self._format, self._wire_format = audio, wire
        self._chunk_samples = audio.sample_rate_hz * CHUNK_MS // 1000
        self._events.bound_audio(second * PLAYBACK_BUFFER_MS // 1000)
        if self._close_follows_deadline:
            self._close_timeout = ready.limits.finalize_deadline_ms / 1000 + CLOSE_MARGIN_S

    async def send_text(self, text: str, *, turn_id: str = "") -> int:
        """Send one complete text segment and return its id.

        The id is the ``segment_id`` of the segment's ``AudioChunk`` and
        ``SegmentCompleted`` events; ids grow by one from 1.

        Raises ``TypeError`` for text that is not a ``str``, and
        ``ValueError`` for text that is empty, only ASCII whitespace (the
        server serves other whitespace) or not encodable as UTF-8 and for a
        `turn_id` that is not at most 64 printable ASCII characters. These
        are raised at once, before any wait and whatever the state of the
        session.

        Waits until the session's input credit admits the segment: its
        bytes, and a place among the segments whose synthesis has not
        started. Also waits until a ``finalize()`` still unanswered has its
        answer. Both come from the server behind the audio it sent first,
        so events must be read meanwhile; when the session ends first, this
        raises its error. After those waits, raises ``ValueError`` and
        sends nothing for text above ``limits.text_to_speech``'s bound on
        one segment or on the segments between two ``finalize()`` calls
        (both in bytes of UTF-8), and for a segment larger than the
        session's whole input credit. A caller cancelled on the turn this
        call fails gets ``CancelledError``, and the call's error is not
        raised.
        """
        if not isinstance(text, str):
            raise TypeError(f"text is a str, not {type(text).__name__}")
        size = len(text.encode())
        if not text.strip(" \t\n\v\f\r"):
            raise ValueError("a text segment holds text that is not only ASCII whitespace")
        if len(turn_id) > 64 or not (turn_id.isascii() and turn_id.isprintable()):
            raise ValueError("a turn id is at most 64 printable ASCII characters")
        return await self._within(self._send_segment(text, turn_id, size), None)

    async def _send_segment(self, text: str, turn_id: str, size: int) -> int:
        async with self._input_gate:
            self._check_sending()
            # Text sent before a Finalize's answer is refused by the server.
            await self._until(lambda: self._finalizing is None)
            limits = self._opened().limits.text_to_speech
            if size > limits.max_segment_text_bytes:
                raise ValueError(
                    f"a text segment holds at most {limits.max_segment_text_bytes} bytes of UTF-8"
                )
            if self._synthesis_text + size > limits.max_synthesis_text_bytes:
                raise ValueError(
                    f"the segments between two finalize() calls hold at most "
                    f"{limits.max_synthesis_text_bytes} bytes of UTF-8 together"
                )
            if size > self._credit_limit:
                raise ValueError("the segment is larger than the session's input credit")
            await self._until(lambda: self._fits(size))
            event = pb.ClientEvent(text_segment=pb.TextSegment(text=text, turn_id=turn_id))
            await self._send(event, charge=size)
            return int(event.text_segment.segment_id)

    def _sending(self, event: pb.ClientEvent) -> None:
        # A segment is counted only once its write is certain, and before its audio can arrive.
        if event.HasField("text_segment"):
            segment = event.text_segment
            segment.segment_id = self._next_segment
            self._next_segment += 1
            self._synthesis_text += len(segment.text.encode())
            self._segments.append(segment.segment_id)

    async def finalize(self, *, timeout: float | None = None) -> SynthesisCompleted:
        """Complete the segments sent so far and return the server's ``SynthesisCompleted``.

        The answer follows the audio and the ``SegmentCompleted`` of every
        segment sent before this call, so events must be read meanwhile;
        iteration yields the answer too, in its place. Until it arrives no
        text is sent: ``send_text()`` waits, also when this call was
        cancelled before its answer came. The bound on the text between two
        ``finalize()`` calls starts again with this one. `timeout`, in
        seconds, bounds the whole call, the wait for its turn included. A
        timeout ends the session: the call is cancelled and
        ``SessionTimeoutError`` raised, here and from every later use. A
        caller cancelled on the turn this call fails gets
        ``CancelledError``, and the call's error is not raised.
        """
        return await self._bounded(self._final(), timeout, "answer to Finalize")

    async def _final(self) -> SynthesisCompleted:
        async with self._input_gate:
            sequence = await self._send_finalize()
            self._synthesis_text = 0
            answer = await self._answer_to(sequence)
            assert isinstance(answer, SynthesisCompleted)
            return answer

    def _task_event(self, event: pb.ServerEvent) -> TextToSpeechEvent | None:
        body = event.WhichOneof("body")
        if body == "audio_chunk":
            return self._chunk(event.audio_chunk)
        if body == "segment_completed":
            done = event.segment_completed
            read = (self._segments[0] if self._segments else None, self._samples, self._chunks)
            if not self._chunks or (done.segment_id, done.total_samples, done.chunk_count) != read:
                raise self._violation("a SegmentCompleted that is not the chunks read")
            self._segments.popleft()
            self._synthesized += 1
            self._synthesized_samples += self._samples
            self._completed += 1
            self._chunks = self._samples = 0
            self._short = False
            return SegmentCompleted(done.segment_id, done.total_samples, done.chunk_count)
        if body == "synthesis_completed":
            answer = event.synthesis_completed
            # No text follows a Finalize before its answer, so every segment sent precedes it.
            if self._segments:
                raise self._violation("a SynthesisCompleted before a segment's completion")
            if (answer.segment_count, answer.total_samples) != (
                self._synthesized,
                self._synthesized_samples,
            ):
                raise self._violation("a SynthesisCompleted that is not the segments completed")
            self._synthesized = self._synthesized_samples = 0
            completed = SynthesisCompleted(
                answer.segment_count, answer.total_samples, answer.finalize_sequence
            )
            self._take_answer(answer.finalize_sequence, completed)
            return completed
        return None

    def _chunk(self, chunk: pb.AudioChunk) -> AudioChunk:
        assert self._format is not None
        if chunk.format != self._wire_format:
            raise self._violation("an audio chunk in another format than Ready's")
        if not self._segments or chunk.segment_id != self._segments[0]:
            raise self._violation("an audio chunk of a segment that is not the oldest unfinished")
        if (chunk.chunk_index, chunk.segment_sample_offset) != (self._chunks, self._samples):
            raise self._violation("an audio chunk out of its place in its segment")
        samples, rest = divmod(len(chunk.data), self._format.sample_width)
        if not samples or rest or samples > self._chunk_samples or self._short:
            raise self._violation(
                f"an audio chunk that is not {CHUNK_MS} ms, or less as its segment's last"
            )
        self._chunks += 1
        self._samples += samples
        self._heard += samples
        self._short = samples < self._chunk_samples
        return AudioChunk(
            chunk.segment_id,
            chunk.chunk_index,
            chunk.segment_sample_offset,
            self._format,
            chunk.data,
        )

    def _check_closed(self, closed: pb.SessionClosed) -> None:
        # A session that ended with a cause reports that cause, whatever its totals say.
        if closed.HasField("cause"):
            return
        if self._segments:
            raise self._violation("a session closed with a segment left without its completion")
        totals = closed.totals
        if (totals.output_audio_samples, totals.segments_completed) != (
            self._heard,
            self._completed,
        ):
            raise self._violation("closing totals that are not the audio and completions read")
