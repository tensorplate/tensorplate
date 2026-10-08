"""The ``faster_whisper`` runner profile: speech-to-text with a Whisper-family model.

It serves any Whisper-family model converted for CTranslate2 that its runner
entry lists, through faster-whisper. The entry names the model directory, the
device and compute type to load it with, the languages the deployment serves
and the input sample rate, and the runner checks each against the loaded model
before it serves; nothing about a particular checkpoint is written here.

A request is one mono clip of 16-bit PCM at the entry's rate, at most one model
input window long, as a one-dimensional ``int16`` tensor named
``audio_frames``, and its language tag as the candidate-only ``text_utf8``
tensor. The transcript comes back as the candidate-only ``result_json`` tensor.
"""

from __future__ import annotations

import gc
import logging
import math
import numbers
import time
from collections.abc import Mapping
from dataclasses import dataclass
from types import MappingProxyType
from typing import Any, Final

from tensorplate_pytorch_backend import sanitize
from tensorplate_pytorch_backend.artifact_set import load_artifact_set
from tensorplate_pytorch_backend.backends.base import (
    Backend,
    BackendError,
    NamedTensor,
    RuntimeCapability,
)
from tensorplate_pytorch_backend.configuration import ArtifactConfigError, read_artifact_config
from tensorplate_pytorch_backend.job_objects import JobRequest, TranscriptResult
from tensorplate_pytorch_backend.protocol import (
    ERR_CONFIG_INVALID,
    ERR_INFERENCE_FAILED,
    ERR_LOAD_FAILED,
    ERR_NOT_READY,
    ERR_OOM_ERROR,
    ERR_SHAPE_MISMATCH,
    ERR_UNSUPPORTED,
    JOB_CLASS_STT_DECODE,
    LIMIT_JOB_INPUT_SAMPLE_RATE_HZ,
)
from tensorplate_pytorch_backend.speech_payload import (
    TEXT_UTF8,
    read_text_utf8,
    result_json_tensor,
)

logger = logging.getLogger("tensorplate.sidecar")

AUDIO_FRAMES: Final[str] = "audio_frames"

#: What every request decodes with: one deterministic greedy pass in the
#: request's language, with word alignment and no prompt history. Voice
#: activity belongs to the serving layer, so faster-whisper's filter stays off.
DECODE_OPTIONS: Final[Mapping[str, Any]] = MappingProxyType(
    {
        "beam_size": 1,
        "temperature": 0.0,
        "condition_on_previous_text": False,
        "vad_filter": False,
        "word_timestamps": True,
    }
)

_ENTRY_FIELDS: Final[frozenset[str]] = frozenset(
    {
        "backend_profile",
        "artifact_set",
        "model_directory",
        "device",
        "compute_type",
        "languages",
        "sample_rate_hz",
    }
)
_DEVICES: Final[frozenset[str]] = frozenset({"cuda", "cpu"})
# CTranslate2 chooses the loaded type for these (for `int8`, its float half from the
# model's saved type: src/types.cc, resolve_compute_type), so they pin nothing.
_UNPINNED_COMPUTE_TYPES: Final[frozenset[str]] = frozenset({"auto", "default", "int8"})
_PRECISION_COMPUTE_TYPES: Final[Mapping[str, frozenset[str]]] = MappingProxyType(
    {
        "fp32": frozenset({"float32"}),
        "fp16": frozenset({"float16"}),
        "bfloat16": frozenset({"bfloat16"}),
        "int8": frozenset({"int8_float32", "int8_float16", "int8_bfloat16"}),
    }
)
# CTranslate2 raises a plain RuntimeError for device allocation failures
# (src/cuda/utils.h: "CUDA failed with error out of memory", and cuBLAS's status).
_OUT_OF_MEMORY_TEXTS: Final[tuple[str, ...]] = ("out of memory", "CUBLAS_STATUS_ALLOC_FAILED")
_PCM_FULL_SCALE: Final[float] = 32768.0


def _failure_code(exc: BaseException, default: str) -> str:
    code = sanitize.code_for(exc, default)
    if code != default or not isinstance(exc, RuntimeError):
        return code
    try:
        text = str(exc)
    except Exception:
        return default
    return ERR_OOM_ERROR if any(marker in text for marker in _OUT_OF_MEMORY_TEXTS) else default


def _read_entry(model_spec: dict[str, Any]) -> dict[str, Any]:
    try:
        entry = read_artifact_config(model_spec)
    except ArtifactConfigError as exc:
        raise BackendError(ERR_CONFIG_INVALID, str(exc)) from exc
    if not entry:
        raise BackendError(
            ERR_CONFIG_INVALID, "the faster_whisper profile needs a JSON runner entry"
        )
    if not set(entry) <= _ENTRY_FIELDS:
        raise BackendError(
            ERR_CONFIG_INVALID,
            "the runner entry has a field the faster_whisper profile does not read",
        )
    if entry.get("backend_profile") != "faster_whisper":
        raise BackendError(
            ERR_CONFIG_INVALID, "the runner entry must select the faster_whisper profile"
        )
    directory = entry.get("model_directory")
    if not isinstance(directory, str) or not directory:
        raise BackendError(ERR_CONFIG_INVALID, "the runner entry must name a model_directory")
    device = entry.get("device")
    if not isinstance(device, str) or device not in _DEVICES:
        raise BackendError(ERR_CONFIG_INVALID, "the runner entry's device must be cuda or cpu")
    compute_type = entry.get("compute_type")
    if (
        not isinstance(compute_type, str)
        or not compute_type
        or compute_type in _UNPINNED_COMPUTE_TYPES
    ):
        raise BackendError(
            ERR_CONFIG_INVALID,
            "the runner entry must pin a compute_type other than auto, default or int8",
        )
    languages = entry.get("languages")
    if (
        not isinstance(languages, list)
        or not languages
        or not all(isinstance(language, str) and language for language in languages)
        or len(set(languages)) != len(languages)
    ):
        raise BackendError(
            ERR_CONFIG_INVALID, "the runner entry must declare one or more distinct languages"
        )
    rate = entry.get("sample_rate_hz")
    if type(rate) is not int or rate <= 0:
        raise BackendError(
            ERR_CONFIG_INVALID, "the runner entry must declare a positive integer sample_rate_hz"
        )
    return entry


def _check_precision_hint(model_spec: dict[str, Any], compute_type: str) -> None:
    hint = model_spec.get("precision_hint", "auto")
    if not isinstance(hint, str):
        raise BackendError(ERR_CONFIG_INVALID, "the model spec's precision_hint must be a string")
    if hint != "auto" and compute_type not in _PRECISION_COMPUTE_TYPES.get(hint, frozenset()):
        raise BackendError(
            ERR_UNSUPPORTED, "the model spec's precision hint is not the entry's compute_type"
        )


def _micros(seconds: object) -> int:
    if (
        not isinstance(seconds, numbers.Real)
        or isinstance(seconds, bool)
        or not math.isfinite(seconds)
        or seconds < 0
    ):
        raise BackendError(
            ERR_INFERENCE_FAILED, "the model returned a time that is not a non-negative number"
        )
    return round(float(seconds) * 1_000_000)


def _interval(start: object, end: object, clip_us: int) -> tuple[int, int]:
    start_us, end_us = _micros(start), _micros(end)
    if end_us < start_us:
        raise BackendError(
            ERR_INFERENCE_FAILED, "the model returned an interval that ends before it starts"
        )
    if start_us > clip_us:
        raise BackendError(
            ERR_INFERENCE_FAILED, "the model returned an interval that starts after the audio"
        )
    # Whisper decodes a zero-padded window, so an interval's end may run into the padding.
    return start_us, min(end_us, clip_us)


def _segment(segment: Any, clip_us: int) -> dict[str, Any]:  # noqa: ANN401 -- a Segment
    start_us, end_us = _interval(segment.start, segment.end, clip_us)
    words = []
    for word in segment.words or ():
        word_start_us, word_end_us = _interval(word.start, word.end, clip_us)
        words.append(
            {
                "start_us": word_start_us,
                "end_us": word_end_us,
                "text": str(word.word),
                "probability": float(word.probability),
            }
        )
    return {
        "start_us": start_us,
        "end_us": end_us,
        "text": str(segment.text),
        "tokens": [int(token) for token in segment.tokens],
        "words": words,
    }


def _audio_payload(inputs: list[NamedTensor], max_samples: int) -> bytes:
    (tensor,) = (item for item in inputs if item.name == AUDIO_FRAMES)
    shape = tensor.tensor.get("shape")
    if (
        tensor.tensor.get("dtype") != "int16"
        or not isinstance(shape, list)
        or len(shape) != 1
        or type(shape[0]) is not int
        or shape[0] * 2 != len(tensor.payload)
    ):
        raise BackendError(
            ERR_SHAPE_MISMATCH, "audio_frames must be a one-dimensional int16 tensor of its payload"
        )
    if not 0 < shape[0] <= max_samples:
        raise BackendError(
            ERR_SHAPE_MISMATCH, "audio_frames must hold one sample to one model input window"
        )
    return tensor.payload


def _inspect(
    model: Any,  # noqa: ANN401 -- faster-whisper's WhisperModel
    *,
    compute_type: str,
    languages: list[str],
    rate: int,
) -> tuple[str, int]:
    """Check the loaded model against its entry; return its compute type and input window."""
    try:
        loaded_compute_type = model.model.compute_type
        model_rate = model.feature_extractor.sampling_rate
        max_samples = model.feature_extractor.n_samples
        supported = set(model.supported_languages)
        tokenizer = model.hf_tokenizer
        undeclared = [
            language
            for language in languages
            if language not in supported or tokenizer.token_to_id(f"<|{language}|>") is None
        ]
    except Exception as exc:
        raise BackendError(ERR_LOAD_FAILED, "the loaded model could not be inspected") from exc
    if loaded_compute_type != compute_type:
        raise BackendError(
            ERR_LOAD_FAILED, "CTranslate2 loaded the model with another compute type"
        )
    if model_rate != rate:
        raise BackendError(
            ERR_CONFIG_INVALID, "the entry's sample_rate_hz is not the model's input rate"
        )
    if undeclared:
        raise BackendError(
            ERR_CONFIG_INVALID, "the entry declares a language the model's tokenizer does not"
        )
    if type(max_samples) is not int or max_samples <= 0:
        raise BackendError(ERR_LOAD_FAILED, "the model's input window could not be read")
    return loaded_compute_type, max_samples


def _release(model: Any) -> None:  # noqa: ANN401 -- faster-whisper's WhisperModel
    # Frees the device copy now rather than whenever the last reference goes.
    try:
        model.model.unload_model()
    except Exception as exc:
        logger.warning("faster_whisper model release failed: %s", sanitize.describe(exc))


@dataclass(frozen=True, slots=True)
class _Loaded:
    """A loaded model and what its entry and load established about it."""

    model: Any
    numpy: Any
    languages: frozenset[str]
    sample_rate_hz: int
    max_samples: int
    runtime: Mapping[str, str]
    load_timings_us: Mapping[str, int]


def _decode(loaded: _Loaded, pcm: bytes, language: str) -> list[dict[str, Any]]:
    """Transcribe one clip of PCM at the model's rate; return its segments."""
    clip_us = -(-(len(pcm) // 2) * 1_000_000 // loaded.sample_rate_hz)
    numpy = loaded.numpy
    try:
        samples = numpy.frombuffer(pcm, dtype="<i2").astype(numpy.float32) / _PCM_FULL_SCALE
        segments, _info = loaded.model.transcribe(samples, language=language, **DECODE_OPTIONS)
        # The generator decodes as it is consumed, so the job ends only here.
        return [_segment(segment, clip_us) for segment in segments]
    except BackendError:
        raise
    except Exception as exc:
        raise BackendError(
            _failure_code(exc, ERR_INFERENCE_FAILED), "the audio could not be transcribed"
        ) from exc


class FasterWhisperBackend(Backend):
    """Transcribes one mono PCM clip per request with a Whisper-family model."""

    def __init__(self) -> None:
        self._loaded: _Loaded | None = None

    @property
    def name(self) -> str:
        return "faster_whisper"

    @property
    def runtime_capability(self) -> RuntimeCapability | None:
        return None

    def load(self, model_spec: dict[str, Any]) -> None:
        started = time.monotonic_ns()
        entry = _read_entry(model_spec)
        device: str = entry["device"]
        compute_type: str = entry["compute_type"]
        languages: list[str] = entry["languages"]
        rate: int = entry["sample_rate_hz"]
        _check_precision_hint(model_spec, compute_type)
        try:
            import ctranslate2
            import faster_whisper
            import numpy
        except Exception as exc:
            raise BackendError(
                ERR_LOAD_FAILED, "faster-whisper or one of its dependencies is not importable"
            ) from exc
        if device == "cuda" and ctranslate2.get_cuda_device_count() < 1:
            raise BackendError(ERR_UNSUPPORTED, "CTranslate2 sees no CUDA device")
        try:
            supported = set(ctranslate2.get_supported_compute_types(device))
        except Exception as exc:
            raise BackendError(
                ERR_LOAD_FAILED, "the device's compute types could not be listed"
            ) from exc
        if compute_type not in supported:
            raise BackendError(
                ERR_UNSUPPORTED, "the device does not support the entry's compute_type"
            )

        verifying = time.monotonic_ns()
        artifacts = load_artifact_set(model_spec["artifact_path"], entry)
        directory_name: str = entry["model_directory"]
        directory = artifacts.directory(directory_name)
        # Without it faster-whisper fetches a tokenizer from the Hugging Face Hub.
        if f"{directory_name}/tokenizer.json" not in artifacts.files:
            raise BackendError(
                ERR_CONFIG_INVALID, "the model directory's artifact_set must list tokenizer.json"
            )
        verified = time.monotonic_ns()
        try:
            model = faster_whisper.WhisperModel(
                str(directory), device=device, compute_type=compute_type, local_files_only=True
            )
        except Exception as exc:
            raise BackendError(
                _failure_code(exc, ERR_LOAD_FAILED), "the Whisper model could not be loaded"
            ) from exc
        built = time.monotonic_ns()
        try:
            loaded_compute_type, max_samples = _inspect(
                model, compute_type=compute_type, languages=languages, rate=rate
            )
        except BaseException:
            _release(model)
            raise
        inspected = time.monotonic_ns()
        self._loaded = _Loaded(
            model=model,
            numpy=numpy,
            languages=frozenset(languages),
            sample_rate_hz=rate,
            max_samples=max_samples,
            runtime=MappingProxyType(
                {
                    "device": device,
                    "compute_type": loaded_compute_type,
                    "faster_whisper": str(getattr(faster_whisper, "__version__", "unknown")),
                    "ctranslate2": str(getattr(ctranslate2, "__version__", "unknown")),
                }
            ),
            load_timings_us=MappingProxyType(
                {
                    "load": (inspected - started) // 1000,
                    "artifact_verify": (verified - verifying) // 1000,
                    "model_build": (built - verified) // 1000,
                }
            ),
        )

    def prime(self) -> None:
        self._require_loaded()

    def infer(self, inputs: list[NamedTensor]) -> list[NamedTensor]:
        loaded = self._require_loaded()
        if len(inputs) != 2 or {item.name for item in inputs} != {AUDIO_FRAMES, TEXT_UTF8}:
            raise BackendError(
                ERR_SHAPE_MISMATCH,
                "a request carries exactly one audio_frames and one text_utf8 tensor",
            )
        language = read_text_utf8(inputs)
        if language not in loaded.languages:
            raise BackendError(
                ERR_UNSUPPORTED, "the request names a language its entry does not declare"
            )
        pcm = _audio_payload(inputs, loaded.max_samples)
        sample_count = len(pcm) // 2
        started = time.monotonic_ns()
        decoded = _decode(loaded, pcm, language)
        decode_us = (time.monotonic_ns() - started) // 1000
        return [
            result_json_tensor(
                {
                    "language": language,
                    "text": "".join(segment["text"] for segment in decoded),
                    "segments": decoded,
                    "sample_rate_hz": loaded.sample_rate_hz,
                    "sample_count": sample_count,
                    "decode_options": dict(DECODE_OPTIONS),
                    "runtime": dict(loaded.runtime),
                    "timings_us": {**loaded.load_timings_us, "decode": decode_us},
                }
            )
        ]

    def infer_async(self, inputs: list[NamedTensor]) -> list[NamedTensor]:
        return self.infer(inputs)

    def cancel(self, request_id: str) -> None:
        _ = request_id

    def unload(self) -> None:
        loaded, self._loaded = self._loaded, None
        if loaded is not None:
            _release(loaded.model)
            del loaded
            gc.collect()

    def job_classes(self) -> tuple[str, ...]:
        loaded = self._loaded
        at_rate = loaded is not None and loaded.sample_rate_hz == LIMIT_JOB_INPUT_SAMPLE_RATE_HZ
        return (JOB_CLASS_STT_DECODE,) if at_rate else ()

    def permits_job(self, request: JobRequest) -> bool:
        loaded = self._loaded  # read once: the reader thread calls this
        return (
            loaded is not None
            and request.job_class == JOB_CLASS_STT_DECODE
            and request.language in loaded.languages
            and len(request.payload) <= 2 * loaded.max_samples
        )

    def run_job(self, request: JobRequest) -> TranscriptResult:
        # Provisional mapping: one batch decode per job. Words are empty because the
        # engine's word records carry no token intervals.
        decoded = _decode(self._require_loaded(), request.payload, request.language)
        text = "".join(segment["text"] for segment in decoded)
        return TranscriptResult(text, [token for segment in decoded for token in segment["tokens"]])

    def _require_loaded(self) -> _Loaded:
        if self._loaded is None:
            raise BackendError(ERR_NOT_READY, "the faster_whisper backend is not loaded")
        return self._loaded


__all__ = ["AUDIO_FRAMES", "DECODE_OPTIONS", "FasterWhisperBackend"]
