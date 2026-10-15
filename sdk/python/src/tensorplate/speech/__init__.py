"""Streaming speech client of the SDK; needs the ``speech`` extra.

Importing this package without ``tensorplate-python[speech]`` installed
raises :class:`~tensorplate.errors.MissingDependencyError`. ``import
tensorplate`` never imports it.
"""

from __future__ import annotations

from tensorplate._extras import require_speech

require_speech()

from tensorplate.speech.client import SpeechClient  # noqa: E402
from tensorplate.speech.discovery import StreamTarget, resolve_stream_target  # noqa: E402
from tensorplate.speech.errors import (  # noqa: E402
    DiscoveryError,
    InvalidEventError,
    NotServingError,
    NotStreamingError,
    PreconditionFailedError,
    RemoteAgentError,
    ResidentSetError,
    ResourceExhaustedError,
    ServerInternalError,
    ServiceUnavailableError,
    SessionCancelledError,
    SessionClosedError,
    SessionTimeoutError,
    SpeechError,
    StreamProtocolError,
)
from tensorplate.speech.events import (  # noqa: E402
    AudioChunk,
    AudioFormat,
    EndpointDetected,
    FinalTranscript,
    PartialTranscript,
    SegmentCompleted,
    SessionLimits,
    SessionTotals,
    SpeechToTextLimits,
    SynthesisCompleted,
    Target,
    TextToSpeechLimits,
    TranscriptSegment,
)
from tensorplate.speech.stt import SpeechToTextEvent, SpeechToTextSession  # noqa: E402
from tensorplate.speech.tts import Follow, TextToSpeechEvent, TextToSpeechSession  # noqa: E402

__all__ = [
    "AudioChunk",
    "AudioFormat",
    "DiscoveryError",
    "EndpointDetected",
    "FinalTranscript",
    "Follow",
    "InvalidEventError",
    "NotServingError",
    "NotStreamingError",
    "PartialTranscript",
    "PreconditionFailedError",
    "RemoteAgentError",
    "ResidentSetError",
    "ResourceExhaustedError",
    "SegmentCompleted",
    "ServerInternalError",
    "ServiceUnavailableError",
    "SessionCancelledError",
    "SessionClosedError",
    "SessionLimits",
    "SessionTimeoutError",
    "SessionTotals",
    "SpeechClient",
    "SpeechError",
    "SpeechToTextEvent",
    "SpeechToTextLimits",
    "SpeechToTextSession",
    "StreamProtocolError",
    "StreamTarget",
    "SynthesisCompleted",
    "Target",
    "TextToSpeechEvent",
    "TextToSpeechLimits",
    "TextToSpeechSession",
    "TranscriptSegment",
    "resolve_stream_target",
]
