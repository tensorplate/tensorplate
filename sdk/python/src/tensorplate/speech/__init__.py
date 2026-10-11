"""Streaming speech client of the SDK; needs the ``speech`` extra.

Importing this package without ``tensorplate-python[speech]`` installed
raises :class:`~tensorplate.errors.MissingDependencyError`. ``import
tensorplate`` never imports it.
"""

from __future__ import annotations

from tensorplate._extras import require_speech

require_speech()

from tensorplate.speech.client import SpeechClient  # noqa: E402
from tensorplate.speech.errors import (  # noqa: E402
    InvalidEventError,
    PreconditionFailedError,
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
    AudioFormat,
    EndpointDetected,
    FinalTranscript,
    PartialTranscript,
    SessionLimits,
    SessionTotals,
    SpeechToTextLimits,
    Target,
    TranscriptSegment,
)
from tensorplate.speech.stt import SpeechToTextEvent, SpeechToTextSession  # noqa: E402

__all__ = [
    "AudioFormat",
    "EndpointDetected",
    "FinalTranscript",
    "InvalidEventError",
    "PartialTranscript",
    "PreconditionFailedError",
    "ResourceExhaustedError",
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
    "Target",
    "TranscriptSegment",
]
