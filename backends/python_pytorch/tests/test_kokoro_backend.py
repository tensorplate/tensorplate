"""CPU fakes for the Kokoro 0.9.4 local model, voice and phoneme APIs."""

from __future__ import annotations

import contextlib
import copy
import dataclasses
import hashlib
import importlib.metadata
import json
import math
import os
import struct
import sys
import weakref
from collections.abc import Callable, Iterable, Iterator
from dataclasses import dataclass, field
from pathlib import Path
from types import ModuleType, SimpleNamespace
from typing import Any

import pytest

from tensorplate_pytorch_backend import job_objects, protocol
from tensorplate_pytorch_backend.backends import kokoro as kokoro_module
from tensorplate_pytorch_backend.backends.base import BackendError, NamedTensor
from tensorplate_pytorch_backend.backends.kokoro import KokoroBackend
from tensorplate_pytorch_backend.runner import default_backend_factories
from tensorplate_pytorch_backend.speech_payload import read_result_json, text_utf8_tensor
from test_faster_whisper_backend import (
    _edge_log,
    _exchange,
    _frame,
    _infer_frame,
    _serving,
    _surfaces,
)
from test_speech_jobs_golden_replay import Peer, golden
from test_speech_jobs_golden_replay import connect as connect  # the fixture
from test_speech_jobs_runner import ACCEPTED, FAILED, digest, ended, exchange, message, read

CANARY = "tp-canary-kokoro-8d27"
_BUNDLE = Path(__file__).resolve().parents[3] / "test/models/bundles/v0_1/tts_kokoro_candidate"
_ENTRY = "tts-kokoro-candidate.json"


@dataclass
class _Engine:
    errors: dict[str, BaseException] = field(default_factory=dict)
    calls: list[tuple[str, Any]] = field(default_factory=list)
    model_refs: list[Any] = field(default_factory=list)
    rate: Any = 24000
    dtype: str = "float32"
    device: str | None = None
    voice_shape: tuple[int, ...] = (510, 1, 256)
    voice_dtype: str = "float32"
    voice_finite: bool = True
    audio: list[float] = field(default_factory=lambda: [-1.5, -1, -0.5, 0, 0.5, 1, 1.5])
    audio_dtype: str = "float32"
    audio_ndim: int = 1
    phonemes: Any = "hello"
    vocab: dict[str, int] = field(default_factory=lambda: dict(zip("helo ", range(5), strict=True)))
    context_length: Any = 512
    fallback: Any = True
    cuda: bool = True
    extra_output: bool = False

    def call(self, stage: str, value: Any = None) -> None:
        self.calls.append((stage, value))
        if stage in self.errors:
            raise self.errors[stage]


class _Array:
    def __init__(self, values: Iterable[float], dtype: str = "float32", ndim: int = 1) -> None:
        self.values, self.dtype, self.ndim = list(values), dtype, ndim
        self.size = len(self.values)

    def all(self) -> bool:
        return all(self.values)

    def sum(self) -> float:
        return sum(self.values)

    def __lt__(self, value: float) -> _Array:
        return _Array([item < value for item in self.values])

    def __gt__(self, value: float) -> _Array:
        return _Array([item > value for item in self.values])

    def __or__(self, other: _Array) -> _Array:
        return _Array([a or b for a, b in zip(self.values, other.values, strict=True)])

    def __mul__(self, value: float) -> _Array:
        return _Array([item * value for item in self.values])

    def astype(self, dtype: str) -> _Array:
        return _Array([int(item) for item in self.values], dtype)

    def tobytes(self) -> bytes:
        assert self.dtype == "<i2"
        return struct.pack(f"<{len(self.values)}h", *self.values)


def _module(name: str, **attrs: Any) -> ModuleType:
    module = ModuleType(name)
    for key, value in attrs.items():
        setattr(module, key, value)
    return module


@pytest.fixture
def engine(monkeypatch: pytest.MonkeyPatch) -> _Engine:
    state = _Engine()
    monkeypatch.setattr(importlib.metadata, "version", lambda name: "3.8.0")
    monkeypatch.setenv("TORCH_FORCE_WEIGHTS_ONLY_LOAD", "0")

    class Tensor:
        def __init__(self, *, voice: bool = False) -> None:
            self.voice = voice
            self.dtype = state.voice_dtype if voice else state.audio_dtype
            self.shape = state.voice_shape if voice else (len(state.audio),)
            self.device = SimpleNamespace(type="cpu")
            self.ndim = len(self.shape) if voice else state.audio_ndim

        def is_floating_point(self) -> bool:
            return True

        def detach(self) -> Tensor:
            state.call("detach")
            return self

        def cpu(self) -> Tensor:
            state.call("cpu")
            return self

        def numpy(self) -> _Array:
            state.call("numpy")
            return _Array(state.audio, self.dtype, self.ndim)

    def torch_load(path: str, *, weights_only: bool, map_location: str | None = None) -> Any:
        assert weights_only is True
        assert os.environ["TORCH_FORCE_WEIGHTS_ONLY_LOAD"] == "1"
        state.call(
            "voice_load" if str(path).endswith(".pt") else "weights_load", (path, map_location)
        )
        return Tensor(voice=True) if str(path).endswith(".pt") else {}

    def clear_cache() -> None:
        state.call("empty_cache", sum(ref() is not None for ref in state.model_refs))

    torch = _module(
        "torch",
        __version__="2.6.0",
        float32="float32",
        Tensor=Tensor,
        load=torch_load,
        isfinite=lambda tensor: _Array([state.voice_finite]),
        inference_mode=contextlib.nullcontext,
        cuda=SimpleNamespace(is_available=lambda: state.cuda, empty_cache=clear_cache),
    )

    class KModel:
        def __init__(self, *, config: dict[str, Any], model: str) -> None:
            state.call("model", (config, model))
            torch_load(model, map_location="cpu", weights_only=True)
            self.vocab = state.vocab
            self.context_length = state.context_length
            self.decoder = SimpleNamespace(
                generator=SimpleNamespace(
                    m_source=SimpleNamespace(l_sin_gen=SimpleNamespace(sampling_rate=state.rate))
                )
            )
            self.device = SimpleNamespace(type="cpu")
            state.model_refs.append(weakref.ref(self))

        def float(self) -> KModel:
            state.call("float")
            return self

        def to(self, device: str) -> KModel:
            state.call("to", device)
            self.device.type = state.device or device
            return self

        def eval(self) -> KModel:
            state.call("eval")
            return self

        def parameters(self) -> Iterator[Any]:
            state.call("parameters")
            return iter(
                [
                    SimpleNamespace(
                        dtype=state.dtype, device=self.device, is_floating_point=lambda: True
                    )
                ]
            )

        def buffers(self) -> Iterator[Any]:
            return iter([])

    class G2P:
        @property
        def fallback(self) -> Any:
            state.call("fallback")
            return state.fallback

        def __call__(self, text: str) -> tuple[Any, list[Any]]:
            state.call("g2p", text)
            return state.phonemes, []

    class KPipeline:
        def __init__(self, *, lang_code: str, model: KModel, trf: bool) -> None:
            assert isinstance(model, KModel)
            assert trf is False
            state.call("pipeline", lang_code)
            self.model, self.g2p = model, G2P()
            self.voices: dict[str, Any] = {}

        def load_voice(self, voice: str) -> Any:
            state.call("voice", voice)
            assert voice.endswith(".pt")
            if voice not in self.voices:
                self.voices[voice] = torch_load(voice, weights_only=True)
            return self.voices[voice]

        def generate_from_tokens(self, tokens: str, *, voice: str, speed: float) -> Iterator[Any]:
            state.call("synthesize", (tokens, voice, speed))
            self.load_voice(voice)
            yield SimpleNamespace(audio=Tensor())
            state.call("exhausted")
            if state.extra_output:
                yield SimpleNamespace(audio=Tensor())

    monkeypatch.setitem(sys.modules, "torch", torch)
    monkeypatch.setitem(
        sys.modules,
        "kokoro",
        _module("kokoro", __version__="0.9.4", KModel=KModel, KPipeline=KPipeline),
    )
    monkeypatch.setitem(
        sys.modules,
        "kokoro.pipeline",
        _module(
            "kokoro.pipeline",
            ALIASES={"en-us": "a", "fr-fr": "f"},
            LANG_CODES={"a": "American English", "f": "fr-fr"},
        ),
    )
    monkeypatch.setitem(
        sys.modules,
        "numpy",
        _module(
            "numpy",
            float32="float32",
            isfinite=lambda a: _Array([math.isfinite(x) for x in a.values]),
            clip=lambda a, lo, hi: _Array([min(hi, max(lo, x)) for x in a.values]),
            rint=lambda a: _Array([round(x) for x in a.values]),
        ),
    )
    return state


def test_engine_fakes_do_not_require_installed_engines(engine: _Engine) -> None:
    import kokoro
    import torch

    assert kokoro.__version__ == "0.9.4"
    assert torch.cuda.is_available()


def _bundle(tmp_path: Path, **changes: Any) -> Path:
    import shutil

    root = tmp_path / CANARY
    shutil.copytree(_BUNDLE, root)
    (root / "model/config.json").write_text(json.dumps({"style_dim": 128}))
    path = root / _ENTRY
    entry = json.loads(path.read_text())
    for item in entry["artifact_set"]:
        item["digest"] = "sha256:" + hashlib.sha256((root / item["path"]).read_bytes()).hexdigest()
    entry.update(changes)
    path.write_text(json.dumps(entry))
    return path


def _spec(path: Path, **extra: Any) -> dict[str, Any]:
    return {
        "schema_version": "0.1",
        "model_id": "tts-kokoro-candidate",
        "model_class": "custom",
        "artifact_path": str(path),
        "backend_hint": "python_pytorch",
        "precision_hint": "auto",
        **extra,
    }


def _loaded(tmp_path: Path, **changes: Any) -> KokoroBackend:
    backend = KokoroBackend()
    backend.load(_spec(_bundle(tmp_path, **changes)))
    backend.prime()
    return backend


def _error(action: Any, code: str) -> BackendError:
    with pytest.raises(BackendError) as exc:
        action()
    assert exc.value.code == code
    assert CANARY not in exc.value.code_message
    return exc.value


def _calls(engine: _Engine, stage: str) -> list[Any]:
    return [value for name, value in engine.calls if name == stage]


def test_local_artifacts_fixed_voice_and_pcm(engine: _Engine, tmp_path: Path) -> None:
    backend = _loaded(tmp_path)
    out = backend.infer([text_utf8_tensor("Hello")])
    result = read_result_json(out)
    assert default_backend_factories()["kokoro"] is KokoroBackend
    assert backend.name == "kokoro"
    assert backend.runtime_capability is None
    assert out[0].name == "audio_frames"
    assert out[0].tensor == {"dtype": "int16", "shape": [7]}
    assert struct.unpack("<7h", out[0].payload) == (-32768, -32768, -16384, 0, 16384, 32767, 32767)
    assert (
        result["sample_rate_hz"],
        result["sample_count"],
        result["duration_us"],
        result["clipped_samples"],
    ) == (24000, 7, 292, 2)
    assert (result["language"], result["voice"], result["dtype"]) == (
        "en-US",
        "af_heart",
        "float32",
    )
    assert result["runtime"] == {
        "device": "cuda",
        "compute_type": "float32",
        "kokoro": "0.9.4",
        "torch": "2.6.0",
    }
    root = tmp_path / CANARY
    assert _calls(engine, "model") == [({"style_dim": 128}, str(root / "model/kokoro-v1_0.pth"))]
    assert _calls(engine, "weights_load") == [(str(root / "model/kokoro-v1_0.pth"), "cpu")]
    assert _calls(engine, "voice_load") == [(str(root / "model/voices/af_heart.pt"), None)]
    assert _calls(engine, "pipeline") == ["en-US"]
    assert _calls(engine, "synthesize") == [("hello", str(root / "model/voices/af_heart.pt"), 1.0)]
    assert _calls(engine, "exhausted") == [None]
    for digest_key, rel in [
        ("config_digest", "model/config.json"),
        ("model_digest", "model/kokoro-v1_0.pth"),
        ("voice_digest", "model/voices/af_heart.pt"),
    ]:
        assert (
            result[digest_key] == "sha256:" + hashlib.sha256((root / rel).read_bytes()).hexdigest()
        )
    assert _calls(engine, "float") == _calls(engine, "eval") == [None]


def test_family_configuration_has_no_reference_model_constants(
    engine: _Engine, tmp_path: Path
) -> None:
    engine.rate = 22050
    backend = _loaded(
        tmp_path,
        languages=["fr-FR"],
        language="fr-FR",
        voice="different",
        voices={"different": {"path": "model/voices/af_heart.pt", "language": "fr-FR"}},
        sample_rate_hz=22050,
        device="cpu",
    )
    result = read_result_json(backend.infer([text_utf8_tensor("Bonjour")]))
    assert (result["language"], result["voice"], result["sample_rate_hz"]) == (
        "fr-FR",
        "different",
        22050,
    )
    assert result["runtime"]["device"] == "cpu"
    backend.unload()
    assert not _calls(engine, "empty_cache")


@pytest.mark.parametrize("field", sorted(kokoro_module._ENTRY_FIELDS))
def test_each_entry_field_is_required(engine: _Engine, tmp_path: Path, field: str) -> None:
    path = _bundle(tmp_path)
    entry = json.loads(path.read_text())
    del entry[field]
    path.write_text(json.dumps(entry))
    _error(lambda: KokoroBackend().load(_spec(path)), "config_invalid")
    assert not engine.calls


@pytest.mark.parametrize(
    ("changes", "code"),
    [
        ({"extra": 1}, "config_invalid"),
        ({"backend_profile": "fixture"}, "config_invalid"),
        ({"language": "zz"}, "unsupported"),
        ({"voice": CANARY}, "unsupported"),
        ({"languages": []}, "config_invalid"),
        ({"languages": ["en-US", "en-US"]}, "config_invalid"),
        ({"languages": [None]}, "config_invalid"),
        ({"languages": "en-US"}, "config_invalid"),
        ({"languages": ["x" * 17]}, "config_invalid"),
        ({"languages": [""]}, "config_invalid"),
        ({"voices": {}}, "config_invalid"),
        ({"voices": []}, "config_invalid"),
        ({"voices": {"x": None}}, "config_invalid"),
        ({"voices": {"x": {"path": "x.pt", "language": "en-US", "extra": 1}}}, "config_invalid"),
        ({"voices": {"x": {"path": "x.pt", "language": "zz"}}}, "config_invalid"),
        ({"voices": {"x": {"path": "x, y.pt", "language": "en-US"}}}, "config_invalid"),
        ({"voices": {"x": {"path": "x.pth", "language": "en-US"}}}, "config_invalid"),
        ({"voices": {"": {"path": "x.pt", "language": "en-US"}}}, "config_invalid"),
        ({"voices": {"x" * 65: {"path": "x.pt", "language": "en-US"}}}, "config_invalid"),
        ({"sample_rate_hz": True}, "config_invalid"),
        ({"sample_rate_hz": 0}, "config_invalid"),
        ({"sample_rate_hz": 24000.0}, "config_invalid"),
        ({"device": "auto"}, "unsupported"),
        ({"device": None}, "config_invalid"),
        ({"model_weights": ""}, "config_invalid"),
        ({"compute_type": "float16"}, "unsupported"),
        ({"compute_type": "auto"}, "unsupported"),
    ],
)
def test_invalid_entry_before_loading(
    engine: _Engine, tmp_path: Path, changes: dict[str, Any], code: str
) -> None:
    _error(lambda: KokoroBackend().load(_spec(_bundle(tmp_path, **changes))), code)
    assert not engine.calls


@pytest.mark.parametrize(
    ("hint", "code"),
    [(None, "config_invalid"), ("fp16", "unsupported"), ("unknown", "unsupported")],
)
def test_precision_hint(engine: _Engine, tmp_path: Path, hint: Any, code: str) -> None:
    _error(lambda: KokoroBackend().load(_spec(_bundle(tmp_path), precision_hint=hint)), code)
    assert not engine.calls


@pytest.mark.parametrize(
    "rel", ["model/config.json", "model/kokoro-v1_0.pth", "model/voices/af_heart.pt"]
)
def test_digest_before_deserialization(engine: _Engine, tmp_path: Path, rel: str) -> None:
    path = _bundle(tmp_path)
    (path.parent / rel).write_text(CANARY)
    _error(lambda: KokoroBackend().load(_spec(path)), "load_failed")
    assert not engine.calls


@pytest.mark.parametrize(
    ("attribute", "value", "code"),
    [
        ("rate", 16000, "config_invalid"),
        ("rate", True, "config_invalid"),
        ("dtype", "float16", "load_failed"),
        ("device", "cpu", "load_failed"),
        ("context_length", 2, "load_failed"),
        ("context_length", None, "load_failed"),
        ("vocab", {}, "load_failed"),
        ("fallback", None, "load_failed"),
        ("voice_shape", (510, 1, 128), "load_failed"),
        ("voice_shape", (510, 256), "load_failed"),
        ("voice_shape", (0, 1, 256), "load_failed"),
        ("voice_dtype", "float16", "load_failed"),
        ("voice_finite", False, "load_failed"),
        ("cuda", False, "unsupported"),
    ],
)
def test_loaded_configuration_must_match(
    engine: _Engine, tmp_path: Path, attribute: str, value: Any, code: str
) -> None:
    setattr(engine, attribute, value)
    backend = KokoroBackend()
    _error(lambda: backend.load(_spec(_bundle(tmp_path))), code)
    _error(backend.prime, "not_ready")
    assert not any(ref() for ref in engine.model_refs)
    assert _calls(engine, "empty_cache") == [0]


def test_missing_packaged_spacy_is_not_downloaded(
    engine: _Engine, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    def missing(name: str) -> str:
        raise importlib.metadata.PackageNotFoundError(CANARY)

    monkeypatch.setattr(importlib.metadata, "version", missing)
    _error(lambda: _loaded(tmp_path), "load_failed")
    assert not _calls(engine, "model")
    assert not _calls(engine, "pipeline")


@pytest.mark.parametrize("text", ["", " \n\t", "a" * 4097, "é" * 2049, "x\0y"])
def test_invalid_text_before_model_work(engine: _Engine, tmp_path: Path, text: str) -> None:
    backend = _loaded(tmp_path)
    _error(lambda: backend.infer([text_utf8_tensor(text)]), "config_invalid")
    assert not _calls(engine, "g2p")
    assert not _calls(engine, "synthesize")


@pytest.mark.parametrize(
    ("inputs", "code"),
    [
        ([], "shape_mismatch"),
        ([text_utf8_tensor("a"), text_utf8_tensor("b")], "shape_mismatch"),
        ([NamedTensor("voice", {"dtype": "uint8", "shape": [1]}, b"a")], "shape_mismatch"),
        ([NamedTensor("text_utf8", {"dtype": "float32", "shape": [1]}, b"a")], "shape_mismatch"),
        ([NamedTensor("text_utf8", {"dtype": "uint8", "shape": [2]}, b"a")], "shape_mismatch"),
        ([NamedTensor("text_utf8", {"dtype": "uint8", "shape": [1]}, b"\xff")], "config_invalid"),
    ],
)
def test_tensor_and_utf8_contract(
    engine: _Engine, tmp_path: Path, inputs: list[NamedTensor], code: str
) -> None:
    _error(lambda: _loaded(tmp_path).infer(inputs), code)
    assert not _calls(engine, "g2p")


@pytest.mark.parametrize(
    ("phonemes", "code"),
    [
        ("", "config_invalid"),
        (" ", "config_invalid"),
        (None, "config_invalid"),
        ("h" * 201, "config_invalid"),
        (CANARY, "unsupported"),
    ],
)
def test_phonemes_before_gpu_execution(
    engine: _Engine, tmp_path: Path, phonemes: Any, code: str
) -> None:
    engine.phonemes = phonemes
    _error(lambda: _loaded(tmp_path).infer([text_utf8_tensor("Hello")]), code)
    assert not _calls(engine, "synthesize")


@pytest.mark.parametrize("limit", ["context", "voice", "profile"])
def test_phoneme_limits_have_accept_and_reject_boundaries(
    engine: _Engine, tmp_path: Path, limit: str
) -> None:
    maximum = 200 if limit == "profile" else 4
    if limit == "context":
        engine.context_length = 6
    if limit == "voice":
        engine.voice_shape = (4, 1, 256)
    backend = _loaded(tmp_path)
    engine.phonemes = "h" * maximum
    backend.infer([text_utf8_tensor("Hello")])
    engine.phonemes += "h"
    _error(lambda: backend.infer([text_utf8_tensor("Hello")]), "config_invalid")
    assert len(_calls(engine, "synthesize")) == 1


@pytest.mark.parametrize(
    ("attribute", "value"),
    [
        ("audio", []),
        ("audio", [math.nan]),
        ("audio", [math.inf]),
        ("audio_ndim", 2),
        ("audio_dtype", "float16"),
        ("extra_output", True),
    ],
)
def test_invalid_waveform(engine: _Engine, tmp_path: Path, attribute: str, value: Any) -> None:
    setattr(engine, attribute, value)
    _error(lambda: _loaded(tmp_path).infer([text_utf8_tensor("Hi")]), "inference_failed")


def test_waveform_duration_limit(engine: _Engine, tmp_path: Path) -> None:
    engine.rate = 3
    backend = _loaded(tmp_path, sample_rate_hz=3)
    engine.audio = [0.0] * 90
    assert read_result_json(backend.infer([text_utf8_tensor("Hi")]))["duration_us"] == 30_000_000
    engine.audio.append(0)
    _error(lambda: backend.infer([text_utf8_tensor("Hi")]), "inference_failed")


def test_pcm_rounding_and_one_sample(engine: _Engine, tmp_path: Path) -> None:
    engine.audio = [0.5 / 32768, 1.5 / 32768, -0.5 / 32768, -1.5 / 32768]
    backend = _loaded(tmp_path)
    out = backend.infer([text_utf8_tensor("Hi")])
    assert struct.unpack("<4h", out[0].payload) == (0, 2, 0, -2)
    engine.audio = [0.0]
    assert read_result_json(backend.infer([text_utf8_tensor("Hi")]))["sample_count"] == 1


def test_scripted_timings_cover_complete_load_and_execution(
    engine: _Engine, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ticks = iter([0, 11_000, 30_000, 71_000, 101_000, 200_000, 217_000, 260_000, 280_000])
    monkeypatch.setattr(
        "tensorplate_pytorch_backend.backends.kokoro.time.monotonic_ns", lambda: next(ticks)
    )
    result = read_result_json(_loaded(tmp_path).infer([text_utf8_tensor("Hi")]))
    assert result["timings_us"] == {
        "load": 101,
        "artifact_verify": 19,
        "model_build": 41,
        "phonemize": 17,
        "synthesize": 43,
        "pcm": 20,
    }


def test_unload_releases_before_allocator_cleanup(engine: _Engine, tmp_path: Path) -> None:
    backend = _loaded(tmp_path)
    backend.infer_async([text_utf8_tensor("Hi")])
    backend.cancel("request")
    backend.unload()
    backend.unload()
    assert _calls(engine, "empty_cache") == [0]
    assert not any(ref() for ref in engine.model_refs)
    _error(backend.prime, "not_ready")
    _error(lambda: backend.infer([text_utf8_tensor("Hi")]), "not_ready")


def test_reload_requires_unload(engine: _Engine, tmp_path: Path) -> None:
    path = _bundle(tmp_path)
    backend = KokoroBackend()
    backend.load(_spec(path))
    _error(lambda: backend.load(_spec(path)), "config_invalid")
    backend.unload()
    backend.load(_spec(path))
    assert len(_calls(engine, "model")) == 2


@pytest.mark.parametrize(
    "stage",
    [
        "model",
        "weights_load",
        "float",
        "to",
        "eval",
        "parameters",
        "pipeline",
        "fallback",
        "voice",
        "voice_load",
        "g2p",
        "synthesize",
        "exhausted",
        "detach",
        "cpu",
        "numpy",
    ],
)
@pytest.mark.parametrize(("error_type", "code"), [(RuntimeError, None), (MemoryError, "oom_error")])
def test_engine_canaries_over_socket(
    engine: _Engine, tmp_path: Path, stage: str, error_type: type[Exception], code: str | None
) -> None:
    path = _bundle(tmp_path)
    engine.errors[stage] = error_type(f"{CANARY} text /voice/{CANARY}.pt")
    load_stage = stage in {
        "model",
        "weights_load",
        "float",
        "to",
        "eval",
        "parameters",
        "pipeline",
        "fallback",
        "voice",
        "voice_load",
    }
    frames = [_frame(protocol.KIND_LOAD_MODEL, model_spec=_spec(path))]
    if not load_stage:
        frames += [_frame(protocol.KIND_PRIME), _infer_frame([text_utf8_tensor(CANARY)])]
    with _serving() as client:
        written, response, log = _surfaces(client, frames)
    assert response.header["error"]["code"] == (
        code or ("load_failed" if load_stage else "inference_failed")
    )
    assert CANARY.encode() not in written
    assert CANARY not in log


def test_allocator_error_is_sanitized(engine: _Engine, tmp_path: Path) -> None:
    backend = _loaded(tmp_path)
    engine.errors["empty_cache"] = RuntimeError(CANARY)
    with _edge_log() as log:
        backend.unload()
    assert "RuntimeError" in log.getvalue()
    assert CANARY not in log.getvalue()


def test_socket_success_and_default_selector_refusal(engine: _Engine, tmp_path: Path) -> None:
    path = _bundle(tmp_path)
    with _serving() as client:
        _, response = _exchange(client, _frame(protocol.KIND_LOAD_MODEL, model_spec=_spec(path)))
        assert response.header["status"] == "ok"
        _, health = _exchange(client, _frame(protocol.KIND_HEALTH_CHECK))
        assert health.header["health"]["backend_factory"] == "kokoro"
        _exchange(client, _frame(protocol.KIND_PRIME))
        _, result = _exchange(client, _infer_frame([text_utf8_tensor("Hi")]))
        assert result.header["status"] == "ok"
        assert len(result.header["tensors"]) == 2
    entry = json.loads(path.read_text())
    del entry["backend_profile"]
    path.write_text(json.dumps(entry))
    with _serving("kokoro") as client:
        _, result = _exchange(client, _frame(protocol.KIND_LOAD_MODEL, model_spec=_spec(path)))
        assert result.header["error"]["code"] == "config_invalid"


def test_voice_language_pair_must_match(engine: _Engine, tmp_path: Path) -> None:
    err = _error(
        lambda: _loaded(tmp_path, languages=["en-US", "fr-FR"], language="fr-FR"), "unsupported"
    )
    assert err.code_message == "the selected voice does not declare the selected language"
    assert not engine.calls


def test_language_registry_checked_before_model_construction(
    engine: _Engine, tmp_path: Path
) -> None:
    err = _error(lambda: _loaded(tmp_path, languages=["en-US", "zz"]), "unsupported")
    assert err.code_message == "the installed phonemizer does not support a declared language"
    assert not _calls(engine, "model")


@pytest.mark.parametrize(
    "config", [[], {}, {"style_dim": True}, {"style_dim": 0}, {"style_dim": "128"}]
)
def test_config_style_dimension_is_validated(engine: _Engine, tmp_path: Path, config: Any) -> None:
    path = _bundle(tmp_path)
    entry = json.loads(path.read_text())
    config_path = path.parent / "model/config.json"
    config_path.write_text(json.dumps(config))
    entry["artifact_set"][0]["digest"] = (
        "sha256:" + hashlib.sha256(config_path.read_bytes()).hexdigest()
    )
    path.write_text(json.dumps(entry))
    err = _error(lambda: KokoroBackend().load(_spec(path)), "config_invalid")
    assert err.code_message == "the model config must declare a positive style_dim"
    assert not _calls(engine, "model")


@pytest.mark.parametrize("variant", ["comma_root", "symlink_suffix"])
def test_resolved_voice_path_cannot_trigger_remote_or_mixed_loading(
    engine: _Engine, tmp_path: Path, variant: str
) -> None:
    path = _bundle(tmp_path / ("comma,root" if variant == "comma_root" else "plain"))
    if variant == "symlink_suffix":
        voice = path.parent / "model/voices/af_heart.pt"
        other = voice.with_suffix(".pth")
        voice.rename(other)
        voice.symlink_to(other.name)
    err = _error(lambda: KokoroBackend().load(_spec(path)), "config_invalid")
    assert err.code_message == "the resolved voice path is not a single local .pt file"
    assert not _calls(engine, "model")
    assert not _calls(engine, "voice_load")


def test_extra_tensor_cannot_override_voice(engine: _Engine, tmp_path: Path) -> None:
    inputs = [
        text_utf8_tensor("Hello"),
        NamedTensor("voice", {"dtype": "uint8", "shape": [1]}, b"x"),
    ]
    err = _error(lambda: _loaded(tmp_path).infer(inputs), "shape_mismatch")
    assert err.code_message == "Kokoro expects exactly one text_utf8 tensor"
    assert not _calls(engine, "g2p")


@pytest.mark.parametrize("stage", ["to", "eval", "pipeline", "voice_load"])
def test_failed_load_releases_even_when_library_retains_exception(
    engine: _Engine, tmp_path: Path, stage: str
) -> None:
    engine.errors[stage] = RuntimeError(CANARY)
    _error(lambda: _loaded(tmp_path), "load_failed")
    assert not any(ref() for ref in engine.model_refs)
    assert _calls(engine, "empty_cache") == [0]


@pytest.mark.parametrize("chaining", ["cause", "context", "cycle"])
def test_failed_load_releases_model_from_retained_exception_chain(
    engine: _Engine, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, chaining: str
) -> None:
    inner, outer = RuntimeError(CANARY), RuntimeError(CANARY)
    engine.errors["inner"] = inner

    def fail(model: Any, device: str) -> None:
        try:
            raise inner
        except RuntimeError:
            if chaining == "cause":
                outer.__cause__ = inner
            if chaining == "cycle":
                inner.__cause__ = outer
            if chaining != "cause":
                raise outer from None
        raise outer

    monkeypatch.setattr(sys.modules["kokoro"].KModel, "to", fail)
    _error(lambda: _loaded(tmp_path), "load_failed")
    assert inner.__traceback__ is None
    assert outer.__traceback__ is None
    assert not any(ref() for ref in engine.model_refs)
    assert _calls(engine, "empty_cache") == [0]


@pytest.mark.parametrize("stage", ["model", "synthesize"])
def test_runtime_oom_message_is_classified_without_publication(
    engine: _Engine, tmp_path: Path, stage: str
) -> None:
    engine.errors[stage] = RuntimeError(f"CUDA out of memory: {CANARY}")
    frames = [_frame(protocol.KIND_LOAD_MODEL, model_spec=_spec(_bundle(tmp_path)))]
    if stage == "synthesize":
        frames += [_frame(protocol.KIND_PRIME), _infer_frame([text_utf8_tensor(CANARY)])]
    with _serving() as client:
        written, response, log = _surfaces(client, frames)
    assert response.header["error"]["code"] == "oom_error"
    assert CANARY.encode() not in written
    assert CANARY not in log


def test_engine_import_canary(
    engine: _Engine, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import builtins

    original = builtins.__import__

    def import_module(name: str, *args: Any, **kwargs: Any) -> Any:
        if name == "kokoro":
            raise RuntimeError(CANARY)
        return original(name, *args, **kwargs)

    monkeypatch.setattr(builtins, "__import__", import_module)
    with _serving() as client:
        written, response, log = _surfaces(
            client, [_frame(protocol.KIND_LOAD_MODEL, model_spec=_spec(_bundle(tmp_path)))]
        )
    assert response.header["error"]["code"] == "load_failed"
    assert CANARY.encode() not in written
    assert CANARY not in log


def test_unselected_voice_still_requires_a_verified_artifact(
    engine: _Engine, tmp_path: Path
) -> None:
    voices = {
        "af_heart": {"language": "en-US", "path": "model/voices/af_heart.pt"},
        "other": {"language": "en-US", "path": "model/voices/unlisted.pt"},
    }
    _error(lambda: _loaded(tmp_path, voices=voices), "config_invalid")
    assert not _calls(engine, "model")


_PCM = (-32768, -32768, -16384, 0, 16384, 32767, 32767)
_OTHER: list[dict[str, Any]] = [
    {"language": "fr-FR"},
    {"voice": "af_bella"},
    {"speed_milli": 999},
    {"speed_milli": 1001},
]


def _submit(text: str = "Hello", job_id: int = 1, **options: Any) -> tuple[dict[str, Any], bytes]:
    """The golden tts_synthesis submit for ``text``, with ``options`` changed."""
    header = copy.deepcopy(golden("tts_synthesis")[0])
    header["job_id"] = job_id
    header["options"].update(options)
    header["input"]["payload_length"] = len(text.encode())
    return header, text.encode()


def _job(text: str = "Hello", **options: Any) -> job_objects.JobRequest:
    return job_objects.read_submit(*_submit(text, **options))


def _job_peer(connect: Callable[..., Peer], entry_path: Path) -> tuple[Peer, dict[str, Any]]:
    """A live runner, and its answer to a load of ``entry_path`` that enables jobs."""
    peer = connect()
    capabilities = [protocol.CAPABILITY_SPEECH_JOBS_V1]
    load = _frame(protocol.KIND_LOAD_MODEL, model_spec=_spec(entry_path), capabilities=capabilities)
    return peer, exchange(peer, load.header)


def test_tts_synthesis_is_declared_for_a_loaded_model_at_the_job_output_rate(
    engine: _Engine, tmp_path: Path
) -> None:
    backend = KokoroBackend()
    assert backend.job_classes() == ()
    backend.load(_spec(_bundle(tmp_path)))
    assert backend.job_classes() == (protocol.JOB_CLASS_TTS_SYNTHESIS,)
    engine.rate = 22050
    assert _loaded(tmp_path / "other", sample_rate_hz=22050).job_classes() == ()


def test_a_job_is_permitted_for_the_entrys_language_and_voice_at_speed_1000(
    engine: _Engine, tmp_path: Path
) -> None:
    backend = _loaded(tmp_path)
    assert backend.permits_job(_job())
    for change in _OTHER:
        assert not backend.permits_job(_job(**change))
    decode = dataclasses.replace(_job(), job_class=protocol.JOB_CLASS_STT_DECODE)
    assert not backend.permits_job(decode)
    assert not KokoroBackend().permits_job(_job())


def test_run_job_returns_the_pcm_and_clipping_of_the_synthesis_infer_runs(
    engine: _Engine, tmp_path: Path
) -> None:
    backend = _loaded(tmp_path)
    result = backend.run_job(_job("Héllo"))
    audio, _ = backend.infer([text_utf8_tensor("Héllo")])
    assert result == job_objects.AudioChunkResult(audio.payload, 2)
    assert struct.unpack("<7h", result.pcm) == _PCM
    voice = str(tmp_path / CANARY / "model/voices/af_heart.pt")
    assert _calls(engine, "g2p") == ["Héllo"] * 2
    assert _calls(engine, "synthesize") == [("hello", voice, 1.0)] * 2
    _error(lambda: KokoroBackend().run_job(_job()), "not_ready")


@pytest.mark.parametrize("text", [" \n\t", "a" * 4097, "é" * 2049, "x\0y"])
def test_run_job_refuses_the_text_infer_refuses(engine: _Engine, tmp_path: Path, text: str) -> None:
    request = dataclasses.replace(_job(), payload=text.encode())
    _error(lambda: _loaded(tmp_path).run_job(request), "config_invalid")
    assert not _calls(engine, "g2p")


def test_a_live_runner_declares_tts_synthesis_and_returns_a_job_as_one_audio_chunk(
    engine: _Engine, connect: Callable[..., Peer], tmp_path: Path
) -> None:
    peer, loaded = _job_peer(connect, _bundle(tmp_path))
    assert (loaded["status"], loaded["job_classes"]) == (protocol.STATUS_OK, ["tts_synthesis"])
    peer.send(*_submit("Hello"))
    accepted, completed, released = (peer.read() for _ in range(3))
    frames = [digest(frame.header) for frame in (accepted, completed, released)]
    assert frames == [(ACCEPTED, 1), *ended(1)]
    assert completed.header["result"] == {
        "type": "audio_chunk",
        "format": {"encoding": "pcm_s16le", "sample_rate_hz": 24000, "channels": 1},
        "clipped_samples": 2,
        "payload_length": 14,
    }
    assert struct.unpack("<7h", completed.payload) == _PCM
    for job_id, change in enumerate(_OTHER, start=2):
        peer.send(*_submit("Hello", job_id, **change))
        assert read(peer, 2) == ended(job_id, FAILED, "unsupported", "job_not_permitted")


def test_a_failed_job_whose_error_names_a_canary_voice_and_path_reaches_no_surface(
    engine: _Engine, connect: Callable[..., Peer], tmp_path: Path
) -> None:
    engine.errors["synthesize"] = RuntimeError(f"{CANARY} voice af_heart at /voices/{CANARY}.pt")
    peer, loaded = _job_peer(connect, _bundle(tmp_path))
    with _edge_log() as log:
        peer.send(*_submit(CANARY))
        frames = [loaded, *(peer.read().header for _ in range(3))]
        frames.append(exchange(peer, message(protocol.KIND_HEALTH_CHECK)))
    assert [digest(frame) for frame in frames[1:4]] == [
        (ACCEPTED, 1),
        *ended(1, FAILED, "inference_failed"),
    ]
    assert frames[2]["error"]["message"] == "the text could not be synthesized"
    assert frames[4]["health"]["last_error"] == "the text could not be synthesized"
    said = json.dumps(frames) + log.getvalue()
    assert CANARY not in said
    assert "af_heart" not in said
