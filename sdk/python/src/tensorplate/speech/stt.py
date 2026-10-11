"""Speech-to-text sessions: audio frames in under input credit, transcripts out."""

from __future__ import annotations

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
from tensorplate.stream.v1.session_pb2_grpc import SessionServiceStub

SpeechToTextEvent = PartialTranscript | EndpointDetected | FinalTranscript

_ENCODINGS = {"pcm_s16le": pb.AUDIO_ENCODING_PCM_S16LE, "mulaw": pb.AUDIO_ENCODING_MULAW}


class SpeechToTextSession(Session[SpeechToTextEvent]):
    """One speech-to-text session, from ``SpeechClient.transcribe``.

    Iterating yields ``PartialTranscript``, ``EndpointDetected`` and
    ``FinalTranscript`` in the order the server sent them. Events not taken
    are held up to 16 KiB; past that the session stops reading until some are.
    """

    def __init__(
        self,
        stub: SessionServiceStub,
        target: Target,
        *,
        language: str,
        input_format: AudioFormat,
        trace_id: str,
        turn_id: str,
        open_timeout: float | None,
    ) -> None:
        super().__init__(stub, target, open_timeout)
        self._language, self._trace_id, self._turn_id = language, trace_id, turn_id
        self._format = input_format
        self._sample_width = input_format.sample_width
        self._samples_sent = 0
        self._short_frame = False
        self._endpoint: int | None = None
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

    async def send_audio(self, data: bytes) -> None:
        """Send one frame of audio in the session's input format.

        Waits until the session's input credit admits it. A frame outside the
        session's frame bounds raises ``ValueError`` and sends nothing; a
        frame shorter than the least one is allowed only as the last before
        ``finalize()`` or ``close()``.
        """
        async with self._input_gate:
            self._check_sending()
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
            await self._until(lambda: self._fits(len(data)), None, "input credit")
            audio = pb.Audio(data=data, sample_count=samples, sample_offset=self._samples_sent)
            await self._send(pb.ClientEvent(audio=audio), charge=len(data))
            self._samples_sent += samples
            self._short_frame = samples * 1000 < limits.min_frame_ms * rate

    async def finalize(self, *, timeout: float | None = None) -> FinalTranscript:
        """End the open utterance and return its final transcript.

        The transcript is also yielded by iteration, in its place. No audio
        is sent until it arrives. `timeout`, in seconds, bounds the wait; when
        it passes the session is ended with ``SessionTimeoutError``.
        """
        async with self._input_gate:
            answer = await self._finalize(timeout)
            self._short_frame = False
            assert isinstance(answer, FinalTranscript)
            return answer

    def _task_event(self, event: pb.ServerEvent) -> SpeechToTextEvent | None:
        body = event.WhichOneof("body")
        if body == "partial_transcript":
            message = event.partial_transcript
            if message.utterance_id in (self._endpoint, self._last_final):
                raise self._violation("a partial transcript after its utterance's endpoint")
            return PartialTranscript(message.utterance_id, message.revision, message.text)
        if body == "endpoint_detected":
            endpoint = event.endpoint_detected
            if self._endpoint is not None:
                raise self._violation("an endpoint before the final transcript of the one before")
            self._endpoint = endpoint.utterance_id
            reason = enum_name(pb.EndpointReason, endpoint.reason)
            return EndpointDetected(endpoint.utterance_id, endpoint.end_sample_offset, reason)
        if body == "final_transcript":
            answer = event.final_transcript
            if self._endpoint != answer.utterance_id:
                raise self._violation("a final transcript without its utterance's endpoint")
            self._endpoint, self._last_final = None, answer.utterance_id
            final = FinalTranscript(
                answer.utterance_id,
                tuple(TranscriptSegment(s.start_us, s.end_us, s.text) for s in answer.segments),
                answer.end_sample_offset,
                answer.finalize_sequence,
            )
            if answer.finalize_sequence:
                if answer.finalize_sequence not in self._finalizes:
                    raise self._violation(
                        "a final transcript answers a Finalize that is not waiting"
                    )
                self._finalizes.discard(answer.finalize_sequence)
                self._answers[answer.finalize_sequence] = final
            return final
        return None
