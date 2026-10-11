"""Speech-to-text sessions: audio frames in under input credit, transcripts out."""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from tensorplate.speech._session import Session, enum_name
from tensorplate.speech.events import (
    AudioFormat,
    EndpointDetected,
    FinalTranscript,
    PartialTranscript,
    Target,
    TranscriptSegment,
)
from tensorplate.stream.v1 import session_pb2 as pb

SpeechToTextEvent = PartialTranscript | EndpointDetected | FinalTranscript

_ENCODINGS = {"pcm_s16le": pb.AUDIO_ENCODING_PCM_S16LE, "mulaw": pb.AUDIO_ENCODING_MULAW}


class SpeechToTextSession(Session[SpeechToTextEvent]):
    """One speech-to-text session, from ``SpeechClient.transcribe``.

    Iterating yields ``PartialTranscript``, ``EndpointDetected`` and
    ``FinalTranscript`` in the order the server sent them. Read them while
    the session sends, in a task of their own: events not taken are held up
    to 16 KiB, and past that the session reads nothing more from the server
    until some are.
    """

    @classmethod
    def _create(
        cls,
        connect: Callable[[], Any],
        target: Target,
        *,
        language: str,
        input_format: AudioFormat,
        trace_id: str,
        turn_id: str,
        open_timeout: float | None,
        close_timeout: float | None,
        cancel_timeout: float,
    ) -> SpeechToTextSession:
        session = cls.__new__(cls)
        session._setup(connect, target, open_timeout, close_timeout, cancel_timeout)
        session._configure(language, input_format, trace_id, turn_id)
        return session

    def _configure(
        self, language: str, input_format: AudioFormat, trace_id: str, turn_id: str
    ) -> None:
        self._language, self._trace_id, self._turn_id = language, trace_id, turn_id
        self._format = input_format
        self._sample_width = input_format.sample_width
        self._samples_sent = 0
        self._short_frame = False
        # Utterances whose endpoint was read and whose final transcript was not.
        self._endpoints: set[int] = set()
        self._last_final = 0

    @property
    def input_format(self) -> AudioFormat:
        return self._format

    def _wire_format(self) -> pb.AudioFormat:
        return pb.AudioFormat(
            encoding=_ENCODINGS[self._format.encoding],
            sample_rate_hz=self._format.sample_rate_hz,
            channels=self._format.channels,
        )

    def _open_body(self, resolved: pb.ResolvedTarget) -> pb.Open:
        return pb.Open(
            resolved=resolved,
            mode=pb.SESSION_MODE_STT_STREAMING,
            language=self._language,
            input_format=self._wire_format(),
            trace_id=self._trace_id,
            turn_id=self._turn_id,
        )

    def _check_ready(self, ready: pb.Ready) -> None:
        if ready.input_format != self._wire_format():
            raise self._violation("Ready accepts another input format than the Open's")
        limits = ready.limits.speech_to_text
        if not 0 < limits.min_frame_ms <= limits.max_frame_ms:
            raise self._violation("Ready declares no frame bounds")

    async def send_audio(self, data: bytes | bytearray | memoryview) -> None:
        """Send one frame of audio in the session's input format.

        `data` is any bytes-like object; one that is not ``bytes`` is copied
        once. Waits until the session's input credit admits the frame, and
        until a ``finalize()`` still unanswered has its answer. Both come
        from the server, so events must be read meanwhile; when the session
        ends first, this raises its error. A frame outside the session's
        frame bounds raises ``ValueError`` and sends nothing; a frame
        shorter than the least one is allowed only as the last before
        ``finalize()`` or ``close()``.
        """
        frame = data if isinstance(data, bytes) else bytes(data)
        await self._within(self._send_frame(frame), None)

    async def _send_frame(self, data: bytes) -> None:
        async with self._input_gate:
            self._check_sending()
            # Audio sent before a Finalize's answer is refused by the server.
            await self._until(lambda: self._finalizing is None)
            limits = self._opened().limits.speech_to_text
            rate = self._format.sample_rate_hz
            samples, rest = divmod(len(data), self._sample_width)
            if not samples or rest:
                raise ValueError("audio is a whole, non-zero number of samples")
            if samples * 1000 > limits.max_frame_ms * rate:
                raise ValueError(f"a frame holds at most {limits.max_frame_ms} ms of audio")
            if self._short_frame:
                raise ValueError(
                    f"a frame shorter than {limits.min_frame_ms} ms is the last before "
                    "finalize() or close()"
                )
            if len(data) > self._credit_limit:
                raise ValueError("the frame is larger than the session's input credit")
            await self._until(lambda: self._fits(len(data)))
            audio = pb.Audio(data=data, sample_count=samples, sample_offset=self._samples_sent)
            await self._send(pb.ClientEvent(audio=audio), charge=len(data))
            self._samples_sent += samples
            self._short_frame = samples * 1000 < limits.min_frame_ms * rate

    async def finalize(self, *, timeout: float | None = None) -> FinalTranscript:
        """End the open utterance and return its final transcript.

        The transcript is also yielded by iteration, in its place. Until it
        arrives no audio is sent: ``send_audio()`` waits, also when this
        call was cancelled before its answer came. The answer is read from
        the server after the events that precede it, so events must be read
        meanwhile. `timeout`, in seconds, bounds the whole call, the wait
        for its turn included. A timeout ends the session: the call is
        cancelled and ``SessionTimeoutError`` raised, here and from every
        later use.
        """
        return await self._bounded(self._final(), timeout, "answer to Finalize")

    async def _final(self) -> FinalTranscript:
        async with self._input_gate:
            sequence = await self._send_finalize()
            # The Finalize ends the utterance on whatever frame came last.
            self._short_frame = False
            answer = await self._answer_to(sequence)
            assert isinstance(answer, FinalTranscript)
            return answer

    def _task_event(self, event: pb.ServerEvent) -> SpeechToTextEvent | None:
        body = event.WhichOneof("body")
        if body == "partial_transcript":
            message = event.partial_transcript
            if message.utterance_id in (*self._endpoints, self._last_final):
                raise self._violation("a partial transcript after its utterance's endpoint")
            return PartialTranscript(message.utterance_id, message.revision, message.text)
        if body == "endpoint_detected":
            endpoint = event.endpoint_detected
            # The next utterance may reach its endpoint before this one's final transcript.
            if endpoint.utterance_id in self._endpoints:
                raise self._violation("a second endpoint of one utterance")
            self._endpoints.add(endpoint.utterance_id)
            reason = enum_name(pb.EndpointReason, endpoint.reason)
            return EndpointDetected(endpoint.utterance_id, endpoint.end_sample_offset, reason)
        if body == "final_transcript":
            answer = event.final_transcript
            if answer.utterance_id not in self._endpoints:
                raise self._violation("a final transcript without its utterance's endpoint")
            self._endpoints.discard(answer.utterance_id)
            self._last_final = answer.utterance_id
            final = FinalTranscript(
                answer.utterance_id,
                tuple(TranscriptSegment(s.start_us, s.end_us, s.text) for s in answer.segments),
                answer.end_sample_offset,
                answer.finalize_sequence,
            )
            # A final transcript that names no Finalize answers none: an automatic endpoint's.
            if answer.finalize_sequence:
                self._take_answer(answer.finalize_sequence, final)
            return final
        return None
