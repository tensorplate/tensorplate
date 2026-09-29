"""CPU fakes for the Kokoro 0.9.4 local model, voice and phoneme APIs."""

from __future__ import annotations

import contextlib
import math
import struct
import sys
import weakref
from dataclasses import dataclass, field
from types import ModuleType, SimpleNamespace
from typing import Any

import pytest


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
    def __init__(self, values, dtype="float32", ndim=1):
        self.values, self.dtype, self.ndim = list(values), dtype, ndim
        self.size = len(self.values)

    def all(self):
        return all(self.values)

    def sum(self):
        return sum(self.values)

    def __lt__(self, value):
        return _Array([item < value for item in self.values])

    def __gt__(self, value):
        return _Array([item > value for item in self.values])

    def __or__(self, other):
        return _Array([a or b for a, b in zip(self.values, other.values, strict=True)])

    def __mul__(self, value):
        return _Array([item * value for item in self.values])

    def astype(self, dtype):
        return _Array([int(item) for item in self.values], dtype)

    def tobytes(self):
        assert self.dtype == "<i2"
        return struct.pack(f"<{len(self.values)}h", *self.values)


def _module(name, **attrs):
    module = ModuleType(name)
    for key, value in attrs.items():
        setattr(module, key, value)
    return module


@pytest.fixture
def engine(monkeypatch):
    state = _Engine()

    class Tensor:
        def __init__(self, *, voice=False):
            self.voice = voice
            self.dtype = state.voice_dtype if voice else state.audio_dtype
            self.shape = state.voice_shape if voice else (len(state.audio),)
            self.device = SimpleNamespace(type="cpu")
            self.ndim = len(self.shape) if voice else state.audio_ndim

        def is_floating_point(self):
            return True

        def detach(self):
            state.call("detach")
            return self

        def cpu(self):
            state.call("cpu")
            return self

        def numpy(self):
            state.call("numpy")
            return _Array(state.audio, self.dtype, self.ndim)

    def torch_load(path, *, weights_only, map_location=None):
        assert weights_only is True
        state.call(
            "voice_load" if str(path).endswith(".pt") else "weights_load", (path, map_location)
        )
        return Tensor(voice=True) if str(path).endswith(".pt") else {}

    def clear_cache():
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
        def __init__(self, *, config, model):
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

        def float(self):
            state.call("float")
            return self

        def to(self, device):
            state.call("to", device)
            self.device.type = state.device or device
            return self

        def eval(self):
            state.call("eval")
            return self

        def parameters(self):
            state.call("parameters")
            return iter(
                [
                    SimpleNamespace(
                        dtype=state.dtype, device=self.device, is_floating_point=lambda: True
                    )
                ]
            )

        def buffers(self):
            return iter([])

    class G2P:
        @property
        def fallback(self):
            state.call("fallback")
            return state.fallback

        def __call__(self, text):
            state.call("g2p", text)
            return state.phonemes, []

    class KPipeline:
        def __init__(self, *, lang_code, model, trf):
            assert isinstance(model, KModel)
            assert trf is False
            state.call("pipeline", lang_code)
            self.model, self.voices, self.g2p = model, {}, G2P()

        def load_voice(self, voice):
            state.call("voice", voice)
            assert voice.endswith(".pt")
            if voice not in self.voices:
                self.voices[voice] = torch_load(voice, weights_only=True)
            return self.voices[voice]

        def generate_from_tokens(self, tokens, *, voice, speed):
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


def test_engine_fakes_do_not_require_installed_engines(engine):
    import kokoro
    import torch

    assert kokoro.__version__ == "0.9.4"
    assert torch.cuda.is_available()
