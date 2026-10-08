"""The typed job seam on the sidecar's side of the socket.

Mirrors the runtime's job value objects (``runtime/src/core/job_request.cpp``,
``job_result.cpp`` and ``job_event.cpp``): what a ``job_submit`` must satisfy,
what a result may hold, and the order of one job's messages. A refusal names
the seam's reason and the checks run in the runtime's order, so both ends of
the socket refuse the same message for the same reason. A structural fault the
seam has no reason for is :class:`MalformedJob`. No threads and no I/O.
"""

from __future__ import annotations

import functools
import re
from collections.abc import Mapping, Sequence
from dataclasses import asdict, dataclass
from typing import Any, Final, TypeAlias

from tensorplate_pytorch_backend import protocol

#: The largest integer a wire field carries.
MAX_WIRE_INTEGER: Final[int] = 2**53 - 1
_INPUT_OF: Final[dict[str, str]] = {
    protocol.JOB_CLASS_STT_DECODE: protocol.JOB_INPUT_AUDIO_FRAMES,
    protocol.JOB_CLASS_TTS_SYNTHESIS: protocol.JOB_INPUT_TEXT_SEGMENT,
    protocol.JOB_CLASS_VAD_FRAMES: protocol.JOB_INPUT_VAD_FRAMES,
}
#: The result type each job class fixes.
RESULT_KIND_OF: Final[dict[str, str]] = {
    protocol.JOB_CLASS_STT_DECODE: protocol.JOB_RESULT_TRANSCRIPT,
    protocol.JOB_CLASS_TTS_SYNTHESIS: protocol.JOB_RESULT_AUDIO_CHUNK,
    protocol.JOB_CLASS_VAD_FRAMES: protocol.JOB_RESULT_VAD,
}
_INPUT_FIELDS: Final[dict[str, tuple[str, ...]]] = {
    protocol.JOB_INPUT_AUDIO_FRAMES: ("format", "start_sample"),
    protocol.JOB_INPUT_TEXT_SEGMENT: (),
    protocol.JOB_INPUT_VAD_FRAMES: ("format", "frame_samples", "frame_count", "utterance_id"),
}
_ENVELOPE: Final[tuple[str, ...]] = ("schema_version", "message_id", "kind")
_IDENTITY: Final[tuple[str, ...]] = ("job_id", "session_key", "generation")
_SUBMIT: Final[tuple[str, ...]] = ("job_class", "progress_limit", "options", "input")
_LANGUAGE = re.compile(r"[A-Za-z]{2,3}(-[A-Za-z0-9]{1,8})*")
_VOICE = re.compile(r"[A-Za-z0-9_.-]+")


class MalformedJob(Exception):
    """A job message or result with a structural fault the seam has no reason for."""


class JobRefused(Exception):
    """A request, result or event the seam refuses, with the seam's reason."""

    def __init__(self, reason: str, code: str = protocol.ERR_CONFIG_INVALID) -> None:
        super().__init__(reason)
        self.reason = reason
        self.code = code


@dataclass(frozen=True, slots=True)
class JobIdentity:
    job_id: int
    session_key: int
    generation: int


@dataclass(frozen=True, slots=True)
class AudioFormat:
    encoding: str
    sample_rate_hz: int
    channels: int


_PCM: Final[str] = protocol.AUDIO_ENCODING_PCM_S16LE
INPUT_FORMAT: Final = AudioFormat(_PCM, protocol.LIMIT_JOB_INPUT_SAMPLE_RATE_HZ, 1)
OUTPUT_FORMAT: Final = AudioFormat(_PCM, protocol.LIMIT_JOB_OUTPUT_SAMPLE_RATE_HZ, 1)


@dataclass(frozen=True, slots=True)
class JobRequest:
    """A validated ``job_submit``; a field its job class does not take is empty or zero."""

    identity: JobIdentity
    job_class: str
    progress_limit: int
    #: PCM samples, or the UTF-8 text of a ``text_segment``.
    payload: bytes
    language: str = ""
    voice: str = ""
    speed_milli: int = 0
    start_sample: int = 0
    frame_samples: int = 0
    frame_count: int = 0
    utterance_id: int = 0


@dataclass(frozen=True, slots=True)
class WordUnit:
    text: str
    token_begin: int
    token_end: int
    start_sample: int
    end_sample: int


@dataclass(frozen=True, slots=True)
class TranscriptResult:
    text: str
    tokens: Sequence[int] = ()
    words: Sequence[WordUnit] = ()


@dataclass(frozen=True, slots=True)
class AudioChunkResult:
    pcm: bytes
    clipped_samples: int = 0
    format: AudioFormat = OUTPUT_FORMAT


@dataclass(frozen=True, slots=True)
class VadResult:
    probabilities: Sequence[float]


JobResult: TypeAlias = TranscriptResult | AudioChunkResult | VadResult


def _require(ok: object, reason: str, code: str = protocol.ERR_CONFIG_INVALID) -> None:
    if not ok:
        raise JobRefused(reason, code)


_in_order = functools.partial(_require, code=protocol.ERR_INFERENCE_FAILED)


def _wire_int(value: object, maximum: int = MAX_WIRE_INTEGER) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not 0 <= value <= maximum:
        raise MalformedJob
    return value


def _wire_text(value: object) -> str:
    if not isinstance(value, str):
        raise MalformedJob
    return value


def _fields(value: object, required: Sequence[str], optional: Sequence[str] = ()) -> Any:  # noqa: ANN401
    """``value`` as an object that has every required field and no unknown one."""
    if not isinstance(value, Mapping) or not set(required) <= set(value) <= {*required, *optional}:
        raise MalformedJob
    return value


def _utf8(text: str) -> tuple[int, bool]:
    """The UTF-8 length of ``text``, and whether it is free of lone surrogates."""
    size = len(text.encode("utf-8", "surrogatepass"))
    return size, size == len(text.encode("utf-8", "ignore"))


def _check_identity(identity: JobIdentity) -> JobIdentity:
    for name in _IDENTITY:
        _require(getattr(identity, name), f"{name}_zero")
    return identity


def _check_pcm(audio_format: AudioFormat, required: AudioFormat, size: int) -> None:
    _require(audio_format == required, "audio_format_unsupported")
    _require(size, "pcm_window_empty")
    _require(size % 2 == 0, "pcm_misaligned")


def read_identity(header: Mapping[str, Any]) -> JobIdentity:
    """The identity a job message carries."""
    return _check_identity(JobIdentity(*(_wire_int(header.get(name)) for name in _IDENTITY)))


def read_cancel(header: Mapping[str, Any]) -> JobIdentity:
    """The identity of a ``job_cancel``, which has no other field."""
    return read_identity(_fields(header, (*_ENVELOPE, *_IDENTITY)))


def read_session_release(header: Mapping[str, Any]) -> tuple[int, int]:
    """The session key and generation of a ``session_release``."""
    session = read_identity({**_fields(header, (*_ENVELOPE, *_IDENTITY[1:])), "job_id": 1})
    return session.session_key, session.generation


def read_submit(header: Mapping[str, Any], payload: bytes) -> JobRequest:
    """Validate a ``job_submit`` and the payload region that came with it."""
    fields = _fields(header, (*_ENVELOPE, *_IDENTITY, *_SUBMIT))
    job_class = _wire_text(fields["job_class"])
    progress_limit = _wire_int(fields["progress_limit"])
    options = _fields(fields["options"], (), ("language", "voice", "speed_milli"))
    language = _wire_text(options.get("language", ""))
    voice = _wire_text(options.get("voice", ""))
    speed_milli = _wire_int(options.get("speed_milli", 0), 0xFFFF)
    source = fields["input"]
    input_type = _wire_text(source.get("type")) if isinstance(source, Mapping) else ""
    if input_type not in _INPUT_FIELDS:
        raise MalformedJob
    source = _fields(source, ("type", "payload_length", *_INPUT_FIELDS[input_type]))
    size = len(payload)
    if _wire_int(source["payload_length"]) != size:
        raise MalformedJob
    numbers = {
        name: _wire_int(source[name]) for name in _INPUT_FIELDS[input_type] if name != "format"
    }
    audio_format = None
    if "format" in source:
        parts = _fields(source["format"], ("encoding", "sample_rate_hz", "channels"))
        audio_format = AudioFormat(
            _wire_text(parts["encoding"]),
            _wire_int(parts["sample_rate_hz"]),
            _wire_int(parts["channels"]),
        )

    # From here on, the runtime's checks in the runtime's order.
    identity = read_identity(fields)
    _require(job_class in _INPUT_OF, "job_class_unknown")
    _require(input_type == _INPUT_OF[job_class], "payload_class_mismatch")
    _require(progress_limit <= protocol.LIMIT_PROGRESS_EVENTS_MAX, "progress_limit_too_large")
    takes_language = job_class != protocol.JOB_CLASS_VAD_FRAMES
    takes_voice = job_class == protocol.JOB_CLASS_TTS_SYNTHESIS
    _require(takes_language or not language, "option_not_applicable")
    _require(takes_voice or not (voice or speed_milli), "option_not_applicable")
    if takes_language:
        _require(_LANGUAGE.fullmatch(language), "language_invalid")
        _require(len(language) <= protocol.LIMIT_LANGUAGE_TAG_MAX_BYTES, "language_invalid")
    if takes_voice:
        _require(_VOICE.fullmatch(voice), "voice_invalid")
        _require(len(voice) <= protocol.LIMIT_VOICE_ID_MAX_BYTES, "voice_invalid")
        _require(speed_milli, "speed_invalid")
    if audio_format is None:
        _require(size, "text_empty")
        _require(size <= protocol.LIMIT_TEXT_SEGMENT_MAX_BYTES, "text_too_large")
        try:
            payload.decode("utf-8")
        except UnicodeDecodeError:
            raise JobRefused("text_invalid_utf8") from None
    else:
        _check_pcm(audio_format, INPUT_FORMAT, size)
    if input_type == protocol.JOB_INPUT_AUDIO_FRAMES:
        _require(size <= protocol.LIMIT_AUDIO_FRAMES_MAX_BYTES, "pcm_too_large")
    if input_type == protocol.JOB_INPUT_VAD_FRAMES:
        frames = numbers["frame_count"]
        in_range = 1 <= frames <= protocol.LIMIT_VAD_FRAMES_MAX_PER_JOB
        _require(in_range, "vad_frame_count_out_of_range")
        _require(size <= protocol.LIMIT_VAD_FRAMES_MAX_BYTES, "pcm_too_large")
        _require(size == frames * numbers["frame_samples"] * 2, "vad_frame_bytes_mismatch")
        _require(numbers["utterance_id"], "utterance_id_zero")
    return JobRequest(
        identity, job_class, progress_limit, payload, language, voice, speed_milli, **numbers
    )


def _render_transcript(result: TranscriptResult) -> dict[str, Any]:
    limit = protocol.LIMIT_TRANSCRIPT_TEXT_MAX_BYTES
    size, well_formed = _utf8(result.text)
    _require(size <= limit, "text_too_large")
    _require(well_formed, "text_invalid_utf8")
    tokens = [_wire_int(token, 0xFFFFFFFF) for token in result.tokens]
    _require(len(tokens) <= protocol.LIMIT_TRANSCRIPT_TOKENS_MAX, "token_count_exceeded")
    words = []
    word_bytes = previous_token_end = previous_start = 0
    for word in result.words:
        size, well_formed = _utf8(word.text)
        _require(size, "word_text_empty")
        _require(well_formed, "text_invalid_utf8")
        word_bytes += size
        _require(word_bytes <= limit, "text_too_large")
        begin, end = _wire_int(word.token_begin), _wire_int(word.token_end)
        start, stop = _wire_int(word.start_sample), _wire_int(word.end_sample)
        _require(begin < end, "word_token_interval_empty")
        _require(end <= len(tokens), "word_token_interval_out_of_range")
        _require(begin >= previous_token_end, "word_token_interval_overlap")
        _require(start <= stop, "word_sample_interval_reversed")
        _require(start >= previous_start, "word_sample_order")
        previous_token_end, previous_start = end, start
        words.append(asdict(WordUnit(word.text, begin, end, start, stop)))
    text = result.text
    return {"type": protocol.JOB_RESULT_TRANSCRIPT, "text": text, "tokens": tokens, "words": words}


def render_result(result: object) -> tuple[dict[str, Any], bytes]:
    """Validate a runner's result; return its wire ``result`` object and payload region."""
    if isinstance(result, TranscriptResult):
        return _render_transcript(result), b""
    if isinstance(result, AudioChunkResult):
        size = len(result.pcm)
        _check_pcm(result.format, OUTPUT_FORMAT, size)
        _require(size <= protocol.LIMIT_AUDIO_CHUNK_MAX_BYTES, "pcm_too_large")
        clipped = _wire_int(result.clipped_samples)
        _require(clipped <= size // 2, "clipped_samples_out_of_range")
        return {
            "type": protocol.JOB_RESULT_AUDIO_CHUNK,
            "format": asdict(result.format),
            "clipped_samples": clipped,
            "payload_length": size,
        }, bytes(result.pcm)
    if isinstance(result, VadResult):
        probabilities = list(result.probabilities)
        in_range = 1 <= len(probabilities) <= protocol.LIMIT_VAD_PROBABILITIES_MAX
        _require(in_range, "vad_probability_count_out_of_range")
        for probability in probabilities:
            if isinstance(probability, bool) or not isinstance(probability, (int, float)):
                raise MalformedJob
            # NaN fails both comparisons, so JSON never has to carry one.
            _require(0 <= probability <= 1, "vad_probability_out_of_range")
        return {"type": protocol.JOB_RESULT_VAD, "probabilities": probabilities}, b""
    raise MalformedJob


def error_object(code: str, message: str, context: str | None = None) -> dict[str, str]:
    """The wire error object, refused when a text exceeds the seam's bound."""
    wire = {"schema_version": protocol.SCHEMA_VERSION, "code": code, "message": message}
    if context is not None:
        wire["context"] = context
    longest = max(_utf8(text)[0] for text in (message, context or ""))
    _require(longest <= protocol.LIMIT_ERROR_TEXT_MAX_BYTES, "error_text_too_large")
    return wire


def job_event(
    kind: str,
    identity: JobIdentity,
    *,
    progress_sequence: int | None = None,
    result: object = None,
    error: Mapping[str, str] | None = None,
) -> tuple[dict[str, Any], bytes]:
    """Build one job message: its header, whose ``message_id`` the writer sets, and its payload.

    Refuses what the runtime's event factories refuse, except an error text past
    its bound: :func:`error_object` refuses that when the caller builds ``error``.
    """
    envelope = {"schema_version": protocol.SCHEMA_VERSION, "message_id": "", "kind": kind}
    fields: dict[str, Any] = {**envelope, **asdict(_check_identity(identity))}
    payload = b""
    if kind == protocol.KIND_JOB_PROGRESS:
        sequence = fields["progress_sequence"] = _wire_int(progress_sequence)
        _require(sequence, "progress_sequence_zero")
        _require(sequence <= protocol.LIMIT_PROGRESS_EVENTS_MAX, "progress_sequence_too_large")
    if kind in (protocol.KIND_JOB_PROGRESS, protocol.KIND_JOB_COMPLETED):
        fields["result"], payload = render_result(result)
    if kind == protocol.KIND_JOB_FAILED:
        if error is None:
            raise MalformedJob
        fields["error"] = dict(error)
    return fields, payload


@dataclass(slots=True)
class JobEventSequence:
    """The order of one job's messages, as the receiver of its stream checks it."""

    identity: JobIdentity
    #: None for a job refused before it had a class.
    result_kind: str | None = None
    progress_limit: int = 0
    vad_frames: int = 0
    cancel_requested: bool = False
    cancel_acknowledged: bool = False
    observed_any: bool = False
    accepted: bool = False
    terminal: bool = False
    released: bool = False
    progress_count: int = 0
    #: Text bytes, word text bytes, tokens, PCM bytes and probabilities so far.
    totals: tuple[int, ...] = (0, 0, 0, 0, 0)

    @classmethod
    def for_request(cls, request: JobRequest) -> JobEventSequence:
        result_kind = RESULT_KIND_OF[request.job_class]
        return cls(request.identity, result_kind, request.progress_limit, request.frame_count)

    def note_cancel_requested(self) -> None:
        self.cancel_requested = True

    def _with_result(self, header: Mapping[str, Any], payload: bytes) -> tuple[int, ...]:
        """The totals once the message's result or fragment is added."""
        _in_order(self.accepted, "not_accepted")
        progress = header["kind"] == protocol.KIND_JOB_PROGRESS
        _in_order(not (progress and self.cancel_acknowledged), "progress_after_cancel_acknowledged")
        result = header["result"]
        _in_order(result["type"] == self.result_kind, "result_kind_mismatch")
        if progress:
            following = self.progress_count + 1
            _in_order(header["progress_sequence"] == following, "progress_sequence_gap")
            _in_order(following <= self.progress_limit, "progress_limit_exceeded")
        added = (
            _utf8(result.get("text", ""))[0],
            sum(_utf8(word["text"])[0] for word in result.get("words", ())),
            len(result.get("tokens", ())),
            len(payload),
            len(result.get("probabilities", ())),
        )
        totals = tuple(have + more for have, more in zip(self.totals, added, strict=True))
        text_bytes, word_bytes, tokens, pcm_bytes, probabilities = totals
        within = (
            max(text_bytes, word_bytes) <= protocol.LIMIT_TRANSCRIPT_TEXT_MAX_BYTES
            and tokens <= protocol.LIMIT_TRANSCRIPT_TOKENS_MAX
            and pcm_bytes <= protocol.LIMIT_AUDIO_CHUNK_MAX_BYTES
        )
        _in_order(within, "result_total_exceeded")
        if self.result_kind == protocol.JOB_RESULT_VAD:
            counted = (
                probabilities <= self.vad_frames if progress else probabilities == self.vad_frames
            )
            _in_order(counted, "vad_result_count_mismatch")
        return totals

    def observe(self, header: Mapping[str, Any], payload: bytes = b"") -> None:
        """Record one message of the job, or refuse it and change nothing."""
        kind = header["kind"]
        identity = JobIdentity(*(header[name] for name in _IDENTITY))
        _in_order(identity == self.identity, "identity_mismatch")
        _in_order(not self.released, "event_after_released")
        _in_order(not self.terminal or kind == protocol.KIND_JOB_RELEASED, "event_after_terminal")
        totals = self.totals
        if kind == protocol.KIND_JOB_ACCEPTED:
            _in_order(not self.observed_any, "accepted_out_of_order")
        elif kind in (protocol.KIND_JOB_PROGRESS, protocol.KIND_JOB_COMPLETED):
            totals = self._with_result(header, payload)
        elif kind == protocol.KIND_JOB_CANCEL_ACKNOWLEDGED:
            _in_order(self.cancel_requested, "cancel_acknowledged_without_cancel")
            _in_order(not self.cancel_acknowledged, "duplicate_cancel_acknowledged")
        elif kind == protocol.KIND_JOB_RELEASED:
            _in_order(self.terminal, "released_before_terminal")
        self.observed_any = True
        self.accepted |= kind == protocol.KIND_JOB_ACCEPTED
        self.progress_count += kind == protocol.KIND_JOB_PROGRESS
        self.terminal |= kind in (protocol.KIND_JOB_COMPLETED, protocol.KIND_JOB_FAILED)
        self.cancel_acknowledged |= kind == protocol.KIND_JOB_CANCEL_ACKNOWLEDGED
        self.released = kind == protocol.KIND_JOB_RELEASED
        self.totals = totals


__all__ = [
    "INPUT_FORMAT",
    "MAX_WIRE_INTEGER",
    "OUTPUT_FORMAT",
    "RESULT_KIND_OF",
    "AudioChunkResult",
    "AudioFormat",
    "JobEventSequence",
    "JobIdentity",
    "JobRefused",
    "JobRequest",
    "JobResult",
    "MalformedJob",
    "TranscriptResult",
    "VadResult",
    "WordUnit",
    "error_object",
    "job_event",
    "read_cancel",
    "read_identity",
    "read_session_release",
    "read_submit",
    "render_result",
]
