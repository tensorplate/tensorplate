"""Values the streaming speech client takes and returns."""

from __future__ import annotations

from dataclasses import dataclass, field

_SAMPLE_BYTES = {"pcm_s16le": 2, "mulaw": 1}


@dataclass(frozen=True)
class Target:
    """The deployment a session addresses.

    A worker started without a deployment generation is addressed by its
    deployment alone: generation zero and an empty digest.
    """

    deployment_id: str
    generation: int = 0
    descriptor_digest: str = ""


@dataclass(frozen=True)
class AudioFormat:
    """Raw audio with no container header; ``encoding`` is ``"pcm_s16le"`` or ``"mulaw"``."""

    encoding: str
    sample_rate_hz: int
    channels: int = 1

    @property
    def sample_width(self) -> int:
        """Bytes one sample takes across every channel."""
        try:
            return _SAMPLE_BYTES[self.encoding] * self.channels
        except KeyError:
            raise ValueError(f"unknown audio encoding {self.encoding!r}") from None


@dataclass(frozen=True)
class SpeechToTextLimits:
    """Bounds on one audio frame and on one utterance, in milliseconds."""

    min_frame_ms: int
    max_frame_ms: int
    max_utterance_ms: int


@dataclass(frozen=True)
class TextToSpeechLimits:
    """Bounds on one text segment, and on the segments between two finalizes together."""

    max_segment_text_bytes: int
    max_segment_audio_ms: int
    max_synthesis_text_bytes: int
    max_synthesis_audio_ms: int


@dataclass(frozen=True)
class SessionLimits:
    """The timers the server runs for a session, in milliseconds, and its mode's bounds."""

    idle_timeout_ms: int
    heartbeat_interval_ms: int
    liveness_timeout_ms: int
    max_duration_ms: int
    finalize_deadline_ms: int
    output_stall_timeout_ms: int
    speech_to_text: SpeechToTextLimits | None = None
    text_to_speech: TextToSpeechLimits | None = None


@dataclass(frozen=True)
class SessionTotals:
    """What a session did, as the server counted it."""

    input_audio_samples: int
    input_text_bytes: int
    output_audio_samples: int
    utterances_completed: int
    segments_completed: int


@dataclass(frozen=True)
class PartialTranscript:
    """The whole current hypothesis of an utterance; a later revision replaces it."""

    utterance_id: int
    revision: int
    text: str


@dataclass(frozen=True)
class EndpointDetected:
    """An utterance ended at ``end_sample_offset``, in input samples from the session's start.

    ``reason`` is ``"silence"``, ``"client_finalize"``, ``"duration_limit"``,
    ``"half_close"``, or ``"unspecified"`` for one this client does not know.
    """

    utterance_id: int
    end_sample_offset: int
    reason: str


@dataclass(frozen=True)
class TranscriptSegment:
    """Text over a half-open interval of source audio, in microseconds from the first sample."""

    start_us: int
    end_us: int
    text: str


@dataclass(frozen=True)
class FinalTranscript:
    """The one transcript of an utterance, never revised.

    ``finalize_sequence`` is zero unless a ``finalize()`` ended the utterance.
    """

    utterance_id: int
    segments: tuple[TranscriptSegment, ...]
    end_sample_offset: int
    finalize_sequence: int

    @property
    def text(self) -> str:
        """The segments' text in order, with nothing added between them."""
        return "".join(segment.text for segment in self.segments)


@dataclass(frozen=True)
class AudioChunk:
    """Audio of one text segment: 20 ms of ``format``, or less as the segment's last chunk.

    ``chunk_index`` counts the segment's chunks from zero, and
    ``segment_sample_offset`` its samples before this chunk.
    """

    segment_id: int
    chunk_index: int
    segment_sample_offset: int
    format: AudioFormat
    data: bytes = field(repr=False)

    @property
    def sample_count(self) -> int:
        """Samples in ``data``, each counted once across the format's channels."""
        return len(self.data) // self.format.sample_width


@dataclass(frozen=True)
class SegmentCompleted:
    """A segment's audio was sent whole: ``total_samples`` in ``chunk_count`` chunks."""

    segment_id: int
    total_samples: int
    chunk_count: int


@dataclass(frozen=True)
class SynthesisCompleted:
    """The answer to a ``finalize()``: the segments completed since the one before it.

    ``total_samples`` is those segments' samples together.
    """

    segment_count: int
    total_samples: int
    finalize_sequence: int
