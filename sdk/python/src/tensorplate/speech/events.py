"""Values the streaming speech client takes and returns."""

from __future__ import annotations

from dataclasses import dataclass

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
class SessionLimits:
    """The timers the server runs for a session, in milliseconds, and its mode's bounds."""

    idle_timeout_ms: int
    heartbeat_interval_ms: int
    liveness_timeout_ms: int
    max_duration_ms: int
    finalize_deadline_ms: int
    output_stall_timeout_ms: int
    speech_to_text: SpeechToTextLimits | None = None


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
