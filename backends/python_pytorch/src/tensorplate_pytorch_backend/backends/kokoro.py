"""Kokoro-family batch synthesis from verified local model and voice artifacts."""

from __future__ import annotations

import gc
import importlib.metadata
import itertools
import json
import logging
import os
import time
from collections.abc import Mapping
from dataclasses import dataclass
from types import MappingProxyType
from typing import Any, Final

from tensorplate_pytorch_backend import sanitize
from tensorplate_pytorch_backend.artifact_set import ArtifactSet, load_artifact_set
from tensorplate_pytorch_backend.backends.base import (
    Backend,
    BackendError,
    NamedTensor,
    RuntimeCapability,
)
from tensorplate_pytorch_backend.configuration import ArtifactConfigError, read_artifact_config
from tensorplate_pytorch_backend.job_objects import AudioChunkResult, JobRequest
from tensorplate_pytorch_backend.protocol import (
    ERR_CONFIG_INVALID,
    ERR_INFERENCE_FAILED,
    ERR_LOAD_FAILED,
    ERR_NOT_READY,
    ERR_OOM_ERROR,
    ERR_SHAPE_MISMATCH,
    ERR_UNSUPPORTED,
    JOB_CLASS_TTS_SYNTHESIS,
    LIMIT_JOB_OUTPUT_SAMPLE_RATE_HZ,
)
from tensorplate_pytorch_backend.speech_payload import read_text_utf8, result_json_tensor

logger = logging.getLogger("tensorplate.sidecar")
AUDIO_FRAMES: Final[str] = "audio_frames"
MAX_TEXT_BYTES: Final[int] = 4096
MAX_TEXT_CHARS: Final[int] = 4096
MAX_PHONEMES: Final[int] = 200
MAX_AUDIO_SECONDS: Final[int] = 30
_ENTRY_FIELDS = frozenset(
    {
        "backend_profile",
        "artifact_set",
        "model_config",
        "model_weights",
        "device",
        "compute_type",
        "languages",
        "language",
        "voices",
        "voice",
        "sample_rate_hz",
    }
)


def _failure_code(exc: BaseException, default: str) -> str:
    if isinstance(exc, BackendError):
        return exc.code
    code = sanitize.code_for(exc, default)
    if code != default or not isinstance(exc, RuntimeError):
        return code
    try:
        message = str(exc).lower()
    except Exception:
        return default
    return ERR_OOM_ERROR if "out of memory" in message else default


def _read_entry(model_spec: dict[str, Any]) -> dict[str, Any]:
    try:
        entry = read_artifact_config(model_spec)
    except ArtifactConfigError:
        raise BackendError(
            ERR_CONFIG_INVALID, "the Kokoro runner entry could not be read"
        ) from None
    if not entry or set(entry) != _ENTRY_FIELDS or entry.get("backend_profile") != "kokoro":
        raise BackendError(
            ERR_CONFIG_INVALID, "the Kokoro runner entry has missing or unknown fields"
        )
    for key in ("model_config", "model_weights", "device", "compute_type", "language", "voice"):
        if not isinstance(entry[key], str) or not entry[key]:
            raise BackendError(ERR_CONFIG_INVALID, "a Kokoro runner entry string is invalid")
    if entry["device"] not in {"cpu", "cuda"} or entry["compute_type"] != "float32":
        raise BackendError(ERR_UNSUPPORTED, "Kokoro requires cpu or cuda with float32 computation")
    hint = model_spec.get("precision_hint", "auto")
    if not isinstance(hint, str):
        raise BackendError(ERR_CONFIG_INVALID, "precision_hint must be a string")
    if hint not in {"auto", "fp32"}:
        raise BackendError(
            ERR_UNSUPPORTED, "precision_hint conflicts with Kokoro float32 computation"
        )
    languages = entry["languages"]
    if (
        not isinstance(languages, list)
        or not languages
        or not all(
            isinstance(item, str) and item and len(item.encode("utf-8")) <= 16 for item in languages
        )
        or len(set(languages)) != len(languages)
    ):
        raise BackendError(
            ERR_CONFIG_INVALID, "the entry must declare distinct bounded language tags"
        )
    voices = entry["voices"]
    if not isinstance(voices, dict) or not voices:
        raise BackendError(ERR_CONFIG_INVALID, "the entry must declare voices")
    for name, voice in voices.items():
        if (
            not name
            or len(name.encode("utf-8")) > 64
            or not isinstance(voice, dict)
            or set(voice) != {"path", "language"}
            or not isinstance(voice["path"], str)
            or not voice["path"].endswith(".pt")
            or "," in voice["path"]
            or voice["language"] not in languages
        ):
            raise BackendError(ERR_CONFIG_INVALID, "a voice declaration is invalid")
    if entry["language"] not in languages or entry["voice"] not in voices:
        raise BackendError(ERR_UNSUPPORTED, "the selected language or voice is not declared")
    if voices[entry["voice"]]["language"] != entry["language"]:
        raise BackendError(
            ERR_UNSUPPORTED, "the selected voice does not declare the selected language"
        )
    rate = entry["sample_rate_hz"]
    if type(rate) is not int or rate <= 0:
        raise BackendError(ERR_CONFIG_INVALID, "sample_rate_hz must be a positive integer")
    return entry


def _clear_cache(torch: Any, device: str) -> None:  # noqa: ANN401 -- optional engine
    gc.collect()
    if device == "cuda":
        try:
            torch.cuda.empty_cache()
        except Exception as exc:
            logger.warning("Kokoro allocator release failed: %s", sanitize.describe(exc))


def _discard_tracebacks(exc: BaseException) -> None:
    pending, visited = [exc], set[int]()
    while pending:
        current = pending.pop()
        if id(current) in visited:
            continue
        visited.add(id(current))
        current.__traceback__ = None
        pending.extend(
            linked for linked in (current.__cause__, current.__context__) if linked is not None
        )


@dataclass(frozen=True, slots=True)
class _Loaded:
    pipeline: Any
    torch: Any
    numpy: Any
    language: str
    voice: str
    voice_path: str
    sample_rate_hz: int
    max_phonemes: int
    vocab: frozenset[str]
    identities: Mapping[str, str]
    runtime: Mapping[str, str]
    load_timings_us: Mapping[str, int]


def _build(
    entry: dict[str, Any],
    artifacts: ArtifactSet,
    torch: Any,  # noqa: ANN401 -- optional engine
    kokoro: Any,  # noqa: ANN401 -- optional engine
    numpy: Any,  # noqa: ANN401 -- optional engine
    started: int,
    verifying: int,
    verified: int,
) -> _Loaded:
    from kokoro.pipeline import ALIASES, LANG_CODES

    if any(ALIASES.get(tag.lower(), tag.lower()) not in LANG_CODES for tag in entry["languages"]):
        raise BackendError(
            ERR_UNSUPPORTED, "the installed phonemizer does not support a declared language"
        )
    # misaki's English dependency downloads this package if it is missing.
    importlib.metadata.version("en_core_web_sm")
    config_path = artifacts.file(entry["model_config"])
    config = json.loads(config_path.read_text(encoding="utf-8"))
    if (
        not isinstance(config, dict)
        or type(config.get("style_dim")) is not int
        or config["style_dim"] <= 0
    ):
        raise BackendError(ERR_CONFIG_INVALID, "the model config must declare a positive style_dim")
    for declared_voice in entry["voices"].values():
        artifacts.file(declared_voice["path"])
    weights_path = artifacts.file(entry["model_weights"])
    voice_path = str(artifacts.file(entry["voices"][entry["voice"]]["path"]))
    # load_voice splits on commas before testing the local .pt suffix.
    if not voice_path.endswith(".pt") or "," in voice_path:
        raise BackendError(
            ERR_CONFIG_INVALID, "the resolved voice path is not a single local .pt file"
        )
    model = kokoro.KModel(config=config, model=str(weights_path)).float().to(entry["device"]).eval()
    built = time.monotonic_ns()
    tensors = list(itertools.chain(model.parameters(), model.buffers()))
    if (
        not tensors
        or model.device.type != entry["device"]
        or any(
            t.device.type != entry["device"] or (t.is_floating_point() and t.dtype != torch.float32)
            for t in tensors
        )
    ):
        raise BackendError(ERR_LOAD_FAILED, "Kokoro loaded a different device or compute type")
    # Kokoro's config omits the rate; the decoder's excitation source carries it.
    rate = model.decoder.generator.m_source.l_sin_gen.sampling_rate
    if type(rate) is not int or rate != entry["sample_rate_hz"]:
        raise BackendError(ERR_CONFIG_INVALID, "sample_rate_hz does not match the loaded decoder")
    if type(model.context_length) is not int or model.context_length <= 2:
        raise BackendError(ERR_LOAD_FAILED, "the model has no usable phoneme context")
    if not isinstance(model.vocab, dict) or not model.vocab:
        raise BackendError(ERR_LOAD_FAILED, "the model has no phoneme vocabulary")
    pipeline = kokoro.KPipeline(lang_code=entry["language"], model=model, trf=False)
    if getattr(pipeline.g2p, "fallback", True) is None:
        raise BackendError(ERR_LOAD_FAILED, "the phonemizer fallback is unavailable")
    voice = pipeline.load_voice(voice_path)
    if (
        not isinstance(voice, torch.Tensor)
        or voice.dtype != torch.float32
        or voice.device.type != "cpu"
        or voice.ndim != 3
        or voice.shape[0] <= 0
        or tuple(voice.shape[1:]) != (1, config["style_dim"] * 2)
        or not torch.isfinite(voice).all()
    ):
        raise BackendError(
            ERR_LOAD_FAILED, "the voice is not a finite float32 style tensor for this model"
        )
    inspected = time.monotonic_ns()
    digests = {item["path"]: item["digest"].lower() for item in entry["artifact_set"]}
    return _Loaded(
        pipeline=pipeline,
        torch=torch,
        numpy=numpy,
        language=entry["language"],
        voice=entry["voice"],
        voice_path=voice_path,
        sample_rate_hz=rate,
        max_phonemes=min(MAX_PHONEMES, model.context_length - 2, voice.shape[0]),
        vocab=frozenset(key for key, value in model.vocab.items() if value is not None),
        identities=MappingProxyType(
            {
                "model_digest": digests[entry["model_weights"]],
                "config_digest": digests[entry["model_config"]],
                "voice_digest": digests[entry["voices"][entry["voice"]]["path"]],
            }
        ),
        runtime=MappingProxyType(
            {
                "device": model.device.type,
                "compute_type": "float32",
                "kokoro": str(kokoro.__version__),
                "torch": str(torch.__version__),
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


class KokoroBackend(Backend):
    """Synthesizes bounded text with the entry's static language and local voice."""

    def __init__(self) -> None:
        self._loaded: _Loaded | None = None

    @property
    def name(self) -> str:
        return "kokoro"

    @property
    def runtime_capability(self) -> RuntimeCapability | None:
        return None

    def load(self, model_spec: dict[str, Any]) -> None:
        if self._loaded is not None:
            raise BackendError(ERR_CONFIG_INVALID, "unload the Kokoro model before loading another")
        started = time.monotonic_ns()
        entry = _read_entry(model_spec)
        verifying = time.monotonic_ns()
        artifacts = load_artifact_set(model_spec["artifact_path"], entry)
        verified = time.monotonic_ns()
        # Both upstream loaders already pass weights_only=True; also forbid an unsafe override.
        os.environ["TORCH_FORCE_WEIGHTS_ONLY_LOAD"] = "1"
        try:
            import kokoro
            import numpy
            import torch
        except Exception as exc:
            raise BackendError(
                _failure_code(exc, ERR_LOAD_FAILED), "Kokoro dependencies are unavailable"
            ) from None
        try:
            if entry["device"] == "cuda" and not torch.cuda.is_available():
                raise BackendError(ERR_UNSUPPORTED, "PyTorch sees no CUDA device")
            self._loaded = _build(
                entry, artifacts, torch, kokoro, numpy, started, verifying, verified
            )
            return
        except Exception as exc:
            code = _failure_code(exc, ERR_LOAD_FAILED)
            message = (
                exc.code_message
                if isinstance(exc, BackendError)
                else "the Kokoro model could not be loaded"
            )
            # Libraries may retain chained errors whose frames still own the model.
            _discard_tracebacks(exc)
        _clear_cache(torch, entry["device"])
        raise BackendError(code, message) from None

    def prime(self) -> None:
        self._require_loaded()

    @staticmethod
    def _synthesize(loaded: _Loaded, text: str) -> tuple[bytes, int, tuple[int, int, int, int]]:
        """Return the PCM of ``text``, how many samples clipped and the clock around each stage."""
        if (
            not text.strip()
            or len(text) > MAX_TEXT_CHARS
            or len(text.encode("utf-8")) > MAX_TEXT_BYTES
            or "\0" in text
        ):
            raise BackendError(
                ERR_CONFIG_INVALID, "text must be nonblank, bounded and contain no NUL"
            )
        started = time.monotonic_ns()
        try:
            phonemes, _ = loaded.pipeline.g2p(text)
            if (
                not isinstance(phonemes, str)
                or not phonemes.strip()
                or len(phonemes) > loaded.max_phonemes
            ):
                raise BackendError(
                    ERR_CONFIG_INVALID,
                    "text has no phonemes or exceeds the model's bounded phoneme input",
                )
            if any(symbol not in loaded.vocab for symbol in phonemes):
                raise BackendError(
                    ERR_UNSUPPORTED, "the model vocabulary cannot represent the phonemized text"
                )
            phonemized = time.monotonic_ns()
            with loaded.torch.inference_mode():
                results = loaded.pipeline.generate_from_tokens(
                    phonemes, voice=loaded.voice_path, speed=1.0
                )
                result = next(results)
                if next(results, None) is not None:
                    raise BackendError(
                        ERR_INFERENCE_FAILED, "one phoneme input produced multiple waveforms"
                    )
                audio = result.audio
                if (
                    not isinstance(audio, loaded.torch.Tensor)
                    or audio.dtype != loaded.torch.float32
                    or audio.ndim != 1
                    or not 0 < audio.shape[0] <= loaded.sample_rate_hz * MAX_AUDIO_SECONDS
                ):
                    raise BackendError(
                        ERR_INFERENCE_FAILED, "the model returned an invalid or overlong waveform"
                    )
                samples = audio.detach().cpu().numpy()
            synthesized = time.monotonic_ns()
            numpy = loaded.numpy
            if not numpy.isfinite(samples).all():
                raise BackendError(ERR_INFERENCE_FAILED, "the model returned nonfinite audio")
            clipped = int(((samples < -1.0) | (samples > 1.0)).sum())
            pcm = (
                numpy.clip(numpy.rint(numpy.clip(samples, -1.0, 1.0) * 32768.0), -32768, 32767)
                .astype("<i2")
                .tobytes()
            )
            converted = time.monotonic_ns()
        except BackendError:
            raise
        except Exception as exc:
            raise BackendError(
                _failure_code(exc, ERR_INFERENCE_FAILED), "the text could not be synthesized"
            ) from None
        return pcm, clipped, (started, phonemized, synthesized, converted)

    def infer(self, inputs: list[NamedTensor]) -> list[NamedTensor]:
        loaded = self._require_loaded()
        if len(inputs) != 1:
            raise BackendError(ERR_SHAPE_MISMATCH, "Kokoro expects exactly one text_utf8 tensor")
        pcm, clipped, clock = self._synthesize(loaded, read_text_utf8(inputs))
        started, phonemized, synthesized, converted = clock
        count = len(pcm) // 2
        return [
            NamedTensor(AUDIO_FRAMES, {"dtype": "int16", "shape": [count]}, pcm),
            result_json_tensor(
                {
                    "language": loaded.language,
                    "voice": loaded.voice,
                    **loaded.identities,
                    "sample_rate_hz": loaded.sample_rate_hz,
                    "sample_count": count,
                    "duration_us": -(-count * 1_000_000 // loaded.sample_rate_hz),
                    "dtype": "float32",
                    "clipped_samples": clipped,
                    "runtime": dict(loaded.runtime),
                    "timings_us": {
                        **loaded.load_timings_us,
                        "phonemize": (phonemized - started) // 1000,
                        "synthesize": (synthesized - phonemized) // 1000,
                        "pcm": (converted - synthesized) // 1000,
                    },
                }
            ),
        ]

    def infer_async(self, inputs: list[NamedTensor]) -> list[NamedTensor]:
        return self.infer(inputs)

    def cancel(self, request_id: str) -> None:
        _ = request_id

    def unload(self) -> None:
        loaded, self._loaded = self._loaded, None
        if loaded is not None:
            torch, device = loaded.torch, loaded.runtime["device"]
            del loaded
            _clear_cache(torch, device)

    def job_classes(self) -> tuple[str, ...]:
        loaded = self._loaded
        at_rate = loaded is not None and loaded.sample_rate_hz == LIMIT_JOB_OUTPUT_SAMPLE_RATE_HZ
        return (JOB_CLASS_TTS_SYNTHESIS,) if at_rate else ()

    def permits_job(self, request: JobRequest) -> bool:
        loaded = self._loaded  # read once: the reader thread calls this
        return (
            loaded is not None
            and request.job_class == JOB_CLASS_TTS_SYNTHESIS
            and request.language == loaded.language
            and request.voice == loaded.voice
            and request.speed_milli == 1000
        )

    def run_job(self, request: JobRequest) -> AudioChunkResult:
        # Provisional mapping: one batch synthesis per job, its segment as one result.
        loaded = self._require_loaded()
        pcm, clipped, _clock = self._synthesize(loaded, request.payload.decode("utf-8"))
        return AudioChunkResult(pcm, clipped)

    def _require_loaded(self) -> _Loaded:
        if self._loaded is None:
            raise BackendError(ERR_NOT_READY, "the Kokoro backend is not loaded")
        return self._loaded


__all__ = ["AUDIO_FRAMES", "KokoroBackend"]
