"""The faster_whisper runner profile, with faster-whisper, CTranslate2 and NumPy faked.

The fakes mirror the faster-whisper 1.2.1 and CTranslate2 4.8 attributes the
runner reads, so CI runs the whole profile without either engine installed.
"""

from __future__ import annotations

import contextlib
import dataclasses
import io
import json
import logging
import shutil
import socket
import struct
import sys
import threading
import uuid
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from pathlib import Path
from types import ModuleType, SimpleNamespace
from typing import Any

import pytest

from tensorplate_pytorch_backend import codec, protocol, sanitize
from tensorplate_pytorch_backend.backends import faster_whisper as faster_whisper_module
from tensorplate_pytorch_backend.backends.base import BackendError, NamedTensor
from tensorplate_pytorch_backend.backends.faster_whisper import (
    AUDIO_FRAMES,
    DECODE_OPTIONS,
    FasterWhisperBackend,
)
from tensorplate_pytorch_backend.job_objects import JobIdentity, JobRequest, TranscriptResult
from tensorplate_pytorch_backend.runner import SidecarRunner, default_backend_factories
from tensorplate_pytorch_backend.speech_payload import (
    TEXT_UTF8,
    read_result_json,
    text_utf8_tensor,
)
from test_speech_jobs_golden_replay import Peer
from test_speech_jobs_golden_replay import connect as connect  # the fixture
from test_speech_jobs_runner import (
    ACCEPTED,
    FAILED,
    digest,
    ended,
    exchange,
    message,
    read,
    submit,
)

_BUNDLE = (
    Path(__file__).resolve().parents[3]
    / "test"
    / "models"
    / "bundles"
    / "v0_1"
    / "stt_whisper_candidate"
)
_ENTRY = "stt-whisper-candidate.json"
CANARY = "tp-canary-7f3a9c"
_CANARY_BYTES = CANARY.encode()
_WHISPER_LANGUAGES = ("en", "ar", "fr", "yue")
_ABSENT = object()


@dataclass
class _Word:
    start: Any
    end: Any
    word: str
    probability: float


@dataclass
class _Segment:
    start: Any
    end: Any
    text: str
    tokens: list[int]
    words: list[_Word] | None


def _default_segments() -> list[_Segment]:
    return [
        _Segment(
            0.0,
            1.1,
            " Hello there.",
            [50365, 2425, 456, 13],
            [_Word(0.0, 0.52, " Hello", 0.75), _Word(0.52, 1.1, " there.", 0.5)],
        ),
        _Segment(1.1, 2.675, " Bye.", [4621, 13], [_Word(1.29, 2.675, " Bye.", 0.25)]),
    ]


@dataclass
class _Engine:
    """What the fake engines do and what the runner asked of them."""

    cuda_devices: int = 1
    compute_types: dict[str, set[str]] = field(
        default_factory=lambda: {
            "cuda": {
                "float32",
                "float16",
                "bfloat16",
                "int8",
                "int8_float32",
                "int8_float16",
                "int8_bfloat16",
            },
            "cpu": {"float32", "int8", "int8_float32", "int16"},
        }
    )
    listing_error: BaseException | None = None
    load_error: BaseException | None = None
    loaded_compute_type: str | None = None
    multilingual: bool = True
    tokens: set[str] = field(default_factory=lambda: {"<|en|>", "<|ar|>", "<|fr|>"})
    tokenizer_error: BaseException | None = None
    sampling_rate: int = 16000
    n_samples: Any = 480_000
    segments: list[_Segment] = field(default_factory=_default_segments)
    transcribe_error: BaseException | None = None
    iteration_error: BaseException | None = None
    release_error: BaseException | None = None
    constructed: list[dict[str, Any]] = field(default_factory=list)
    transcribed: list[tuple[list[float], dict[str, Any]]] = field(default_factory=list)
    exhausted: bool = False
    released: int = 0
    queried: int = 0


class _Array:
    """The part of a NumPy array the runner touches."""

    def __init__(self, values: list[float], dtype: str) -> None:
        self.values = values
        self.dtype = dtype

    def astype(self, dtype: str) -> _Array:
        return _Array([float(value) for value in self.values], dtype)

    def __truediv__(self, divisor: float) -> _Array:
        return _Array([value / divisor for value in self.values], self.dtype)


def _frombuffer(buffer: bytes, dtype: str) -> _Array:
    assert dtype == "<i2"
    return _Array(list(struct.unpack(f"<{len(buffer) // 2}h", buffer)), "int16")


def _module(name: str, **attributes: Any) -> ModuleType:
    module = ModuleType(name)
    for key, value in attributes.items():
        setattr(module, key, value)
    return module


def _whisper_model_class(engine: _Engine) -> type:
    class Tokenizer:
        def token_to_id(self, token: str) -> int | None:
            if engine.tokenizer_error is not None:
                raise engine.tokenizer_error
            return 50259 if token in engine.tokens else None

    class CTranslate2Whisper:
        def __init__(self, compute_type: str) -> None:
            self.compute_type = compute_type
            self.is_multilingual = engine.multilingual

        def unload_model(self) -> None:
            engine.released += 1
            if engine.release_error is not None:
                raise engine.release_error

    class WhisperModel:
        def __init__(self, model_size_or_path: str, **kwargs: Any) -> None:
            engine.constructed.append({"path": model_size_or_path, **kwargs})
            if engine.load_error is not None:
                raise engine.load_error
            # As CTranslate2 resolves `int8`: its float half follows the model's saved type,
            # here float16 on cuda and float32 on cpu.
            resolved = {"int8": f"int8_float{16 if kwargs['device'] == 'cuda' else 32}"}.get(
                kwargs["compute_type"], kwargs["compute_type"]
            )
            self.model = CTranslate2Whisper(engine.loaded_compute_type or resolved)
            self.feature_extractor = SimpleNamespace(
                sampling_rate=engine.sampling_rate, n_samples=engine.n_samples
            )
            self.hf_tokenizer = Tokenizer()

        @property
        def supported_languages(self) -> list[str]:
            return list(_WHISPER_LANGUAGES) if self.model.is_multilingual else ["en"]

        def transcribe(self, audio: _Array, **options: Any) -> tuple[Iterator[_Segment], Any]:
            logging.getLogger("faster_whisper").warning("processing audio for %s", CANARY)
            engine.transcribed.append((list(audio.values), options))
            if engine.transcribe_error is not None:
                raise engine.transcribe_error

            def generate() -> Iterator[_Segment]:
                for segment in engine.segments:
                    yield segment
                    if engine.iteration_error is not None:
                        raise engine.iteration_error
                engine.exhausted = True

            return generate(), SimpleNamespace(language=options.get("language"))

    return WhisperModel


@pytest.fixture
def engine(monkeypatch: pytest.MonkeyPatch) -> _Engine:
    state = _Engine()
    monkeypatch.setitem(
        sys.modules,
        "faster_whisper",
        _module("faster_whisper", __version__="1.2.1", WhisperModel=_whisper_model_class(state)),
    )

    def get_cuda_device_count() -> int:
        state.queried += 1
        return state.cuda_devices

    def get_supported_compute_types(device: str) -> set[str]:
        state.queried += 1
        if state.listing_error is not None:
            raise state.listing_error
        return set(state.compute_types[device])

    monkeypatch.setitem(
        sys.modules,
        "ctranslate2",
        _module(
            "ctranslate2",
            __version__="4.8.2",
            get_cuda_device_count=get_cuda_device_count,
            get_supported_compute_types=get_supported_compute_types,
        ),
    )
    monkeypatch.setitem(
        sys.modules, "numpy", _module("numpy", frombuffer=_frombuffer, float32="float32")
    )
    return state


def _bundle(tmp_path: Path, *, under: str = "bundle", **entry_changes: Any) -> Path:
    """Copy the candidate bundle and return its entry, with ``entry_changes`` applied."""
    root = tmp_path / under
    shutil.copytree(_BUNDLE, root)
    entry_path = root / _ENTRY
    if entry_changes:
        entry = json.loads(entry_path.read_text(encoding="utf-8"))
        for key, value in entry_changes.items():
            if value is _ABSENT:
                entry.pop(key, None)
            else:
                entry[key] = value
        entry_path.write_text(json.dumps(entry), encoding="utf-8")
    return entry_path


def _spec(entry_path: Path | str, **extra: Any) -> dict[str, Any]:
    spec: dict[str, Any] = {
        "schema_version": "0.1",
        "model_id": "stt-whisper-candidate",
        "model_class": "custom",
        "artifact_path": str(entry_path),
        "backend_hint": "python_pytorch",
        "precision_hint": "auto",
    }
    spec.update(extra)
    return spec


def _audio(samples: list[int], **tensor: Any) -> NamedTensor:
    payload = struct.pack(f"<{len(samples)}h", *samples)
    metadata: dict[str, Any] = {"dtype": "int16", "shape": [len(samples)]}
    metadata.update(tensor)
    return NamedTensor(name=AUDIO_FRAMES, tensor=metadata, payload=payload)


_THREE_SECONDS = [0] * 48_000


def _request(language: str = "ar", samples: list[int] | None = None) -> list[NamedTensor]:
    return [_audio(_THREE_SECONDS if samples is None else samples), text_utf8_tensor(language)]


def _loaded(tmp_path: Path, **entry_changes: Any) -> FasterWhisperBackend:
    backend = FasterWhisperBackend()
    backend.load(_spec(_bundle(tmp_path, **entry_changes)))
    backend.prime()
    return backend


def _refusal(action: Any) -> BackendError:
    with pytest.raises(BackendError) as caught:
        action()
    return caught.value


@contextlib.contextmanager
def _edge_log() -> Iterator[io.StringIO]:
    stream = io.StringIO()
    handler = logging.StreamHandler(stream)
    handler.addFilter(sanitize.EdgeLogFilter())
    handler.setFormatter(logging.Formatter("%(levelname)s:%(name)s:%(message)s"))
    root = logging.getLogger()
    previous = root.level
    root.addHandler(handler)
    root.setLevel(logging.DEBUG)
    try:
        yield stream
    finally:
        root.removeHandler(handler)
        root.setLevel(previous)


def _frame(kind: str, **extra: Any) -> codec.SidecarFrame:
    header: dict[str, Any] = {
        "schema_version": protocol.SCHEMA_VERSION,
        "message_id": uuid.uuid4().hex,
        "kind": kind,
    }
    header.update(extra)
    return codec.SidecarFrame(header=header)


def _infer_frame(inputs: list[NamedTensor]) -> codec.SidecarFrame:
    frame = _frame(protocol.KIND_INFER, correlation_id="r")
    tensors: list[dict[str, Any]] = []
    payload = bytearray()
    for item in inputs:
        tensors.append(
            {
                "name": item.name,
                "tensor": item.tensor,
                "payload_offset": len(payload),
                "payload_length": len(item.payload),
            }
        )
        payload.extend(item.payload)
    frame.header["tensors"] = tensors
    frame.payload = bytes(payload)
    return frame


def _exchange(client: socket.socket, frame: codec.SidecarFrame) -> tuple[bytes, codec.SidecarFrame]:
    client.sendall(codec.encode(frame))
    buf = bytearray()
    while True:
        try:
            decoded, consumed = codec.decode_one(bytes(buf))
        except codec.IncompleteFrame:
            chunk = client.recv(65536)
            if not chunk:
                raise AssertionError("runner closed before responding") from None
            buf.extend(chunk)
            continue
        return bytes(buf[:consumed]), decoded


@contextlib.contextmanager
def _serving(default_backend: str = "fixture") -> Iterator[socket.socket]:
    client, server = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    runner = SidecarRunner(server, default_backend_name=default_backend)
    thread = threading.Thread(target=runner.serve_forever, daemon=True)
    thread.start()
    try:
        yield client
    finally:
        with contextlib.suppress(OSError):
            client.shutdown(socket.SHUT_RDWR)
        client.close()
        thread.join(timeout=5.0)


@pytest.fixture
def sidecar(engine: _Engine) -> Iterator[socket.socket]:
    with _serving() as client:
        yield client


def _surfaces(
    client: socket.socket, frames: list[codec.SidecarFrame]
) -> tuple[bytes, codec.SidecarFrame, str]:
    """Send ``frames``; return every byte the runner wrote, the last response and the log."""
    with _edge_log() as log:
        written = bytearray()
        response = None
        for frame in frames:
            raw, response = _exchange(client, frame)
            written.extend(raw)
        raw, health = _exchange(client, _frame(protocol.KIND_HEALTH_CHECK))
        written.extend(raw)
    assert response is not None
    assert health.header["status"] == protocol.STATUS_OK
    return bytes(written), response, log.getvalue()


def _load_frame(entry_path: Path) -> codec.SidecarFrame:
    return _frame(protocol.KIND_LOAD_MODEL, model_spec=_spec(entry_path))


def _canary_bundle(tmp_path: Path, **entry_changes: Any) -> Path:
    return _bundle(tmp_path, under=f"{CANARY}/bundle", **entry_changes)


# ----------------------------------------------------------------------
# selection and load
# ----------------------------------------------------------------------


def test_the_candidate_entry_selects_the_faster_whisper_profile(
    engine: _Engine, sidecar: socket.socket, tmp_path: Path
) -> None:
    assert default_backend_factories()["faster_whisper"] is FasterWhisperBackend
    _, loaded = _exchange(sidecar, _load_frame(_bundle(tmp_path)))
    assert loaded.header["status"] == protocol.STATUS_OK
    _, health = _exchange(sidecar, _frame(protocol.KIND_HEALTH_CHECK))
    assert health.header["health"]["ready"] is True
    assert health.header["health"]["backend_factory"] == "faster_whisper"


def test_the_default_backend_selector_cannot_reach_the_profile(
    engine: _Engine, tmp_path: Path
) -> None:
    entry_path = _bundle(tmp_path, backend_profile=_ABSENT)
    with _serving(default_backend="faster_whisper") as client:
        _, response = _exchange(client, _load_frame(entry_path))
    assert response.header["error"] == {
        "schema_version": protocol.SCHEMA_VERSION,
        "code": protocol.ERR_CONFIG_INVALID,
        "message": "the runner entry must select the faster_whisper profile",
    }
    assert engine.constructed == []


def test_load_hands_the_verified_model_directory_to_faster_whisper(
    engine: _Engine, tmp_path: Path
) -> None:
    entry_path = _bundle(tmp_path)
    FasterWhisperBackend().load(_spec(entry_path))
    assert engine.constructed == [
        {
            "path": str((entry_path.parent / "model").resolve()),
            "device": "cuda",
            "compute_type": "float16",
            "local_files_only": True,
        }
    ]


@pytest.mark.parametrize(
    ("changes", "message"),
    [
        ({"extra": 1}, "the runner entry has a field the faster_whisper profile does not read"),
        *(
            ({"backend_profile": value}, "the runner entry must select the faster_whisper profile")
            for value in (_ABSENT, "kokoro", None)
        ),
        ({"model_directory": _ABSENT}, "the runner entry must name a model_directory"),
        ({"model_directory": ""}, "the runner entry must name a model_directory"),
        ({"model_directory": ["model"]}, "the runner entry must name a model_directory"),
        ({"device": _ABSENT}, "the runner entry's device must be cuda or cpu"),
        ({"device": "auto"}, "the runner entry's device must be cuda or cpu"),
        ({"device": ["cuda"]}, "the runner entry's device must be cuda or cpu"),
        *(
            (
                {"compute_type": value},
                "the runner entry must pin a compute_type other than auto, default or int8",
            )
            for value in (_ABSENT, "", "auto", "default", "int8", 16)
        ),
        *(
            ({"languages": value}, "the runner entry must declare one or more distinct languages")
            for value in (_ABSENT, [], "en", ["en", "en"], ["en", ""], ["en", 1])
        ),
        *(
            (
                {"sample_rate_hz": value},
                "the runner entry must declare a positive integer sample_rate_hz",
            )
            for value in (_ABSENT, 0, -16000, 16000.0, True, "16000")
        ),
    ],
)
def test_entry_fields_are_checked_before_the_engine_is_touched(
    engine: _Engine, tmp_path: Path, changes: dict[str, Any], message: str
) -> None:
    err = _refusal(lambda: FasterWhisperBackend().load(_spec(_bundle(tmp_path, **changes))))
    assert (err.code, err.code_message) == (protocol.ERR_CONFIG_INVALID, message)
    assert (engine.queried, engine.constructed) == (0, [])


def test_a_model_spec_without_a_json_entry_is_refused(engine: _Engine) -> None:
    err = _refusal(lambda: FasterWhisperBackend().load(_spec("/dev/null")))
    assert (err.code, err.code_message) == (
        protocol.ERR_CONFIG_INVALID,
        "the faster_whisper profile needs a JSON runner entry",
    )


@pytest.mark.parametrize(
    ("hint", "compute_type"),
    [
        (_ABSENT, "float16"),
        ("auto", "float16"),
        ("fp16", "float16"),
        ("fp32", "float32"),
        ("bfloat16", "bfloat16"),
        ("int8", "int8_float32"),
        ("int8", "int8_float16"),
        ("int8", "int8_bfloat16"),
    ],
)
def test_a_precision_hint_the_entrys_compute_type_honours_loads(
    engine: _Engine, tmp_path: Path, hint: Any, compute_type: str
) -> None:
    spec = _spec(_bundle(tmp_path, compute_type=compute_type))
    if hint is _ABSENT:
        del spec["precision_hint"]
    else:
        spec["precision_hint"] = hint
    FasterWhisperBackend().load(spec)
    assert [call["compute_type"] for call in engine.constructed] == [compute_type]


@pytest.mark.parametrize(
    ("hint", "compute_type"),
    [
        ("fp32", "float16"),
        ("fp16", "float32"),
        ("fp16", "int8_float16"),
        ("bfloat16", "float16"),
        ("int8", "float16"),
        ("int4", "float16"),
        ("float16", "float16"),
    ],
)
def test_a_precision_hint_the_entry_cannot_honour_is_unsupported(
    engine: _Engine, tmp_path: Path, hint: str, compute_type: str
) -> None:
    entry_path = _bundle(tmp_path, compute_type=compute_type)
    err = _refusal(lambda: FasterWhisperBackend().load(_spec(entry_path, precision_hint=hint)))
    assert (err.code, err.code_message) == (
        protocol.ERR_UNSUPPORTED,
        "the model spec's precision hint is not the entry's compute_type",
    )
    assert (engine.queried, engine.constructed) == (0, [])


@pytest.mark.parametrize("hint", [None, 16, ["fp16"]])
def test_a_precision_hint_that_is_not_a_string_is_config_invalid(
    engine: _Engine, tmp_path: Path, hint: Any
) -> None:
    err = _refusal(
        lambda: FasterWhisperBackend().load(_spec(_bundle(tmp_path), precision_hint=hint))
    )
    assert (err.code, err.code_message) == (
        protocol.ERR_CONFIG_INVALID,
        "the model spec's precision_hint must be a string",
    )
    assert (engine.queried, engine.constructed) == (0, [])


@pytest.mark.parametrize("missing", ["faster_whisper", "ctranslate2", "numpy"])
def test_a_missing_engine_is_load_failed(
    engine: _Engine, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, missing: str
) -> None:
    monkeypatch.setitem(sys.modules, missing, None)
    err = _refusal(lambda: FasterWhisperBackend().load(_spec(_bundle(tmp_path))))
    assert (err.code, err.code_message) == (
        protocol.ERR_LOAD_FAILED,
        "faster-whisper or one of its dependencies is not importable",
    )


def test_no_cuda_device_is_unsupported(engine: _Engine, tmp_path: Path) -> None:
    engine.cuda_devices = 0
    err = _refusal(lambda: FasterWhisperBackend().load(_spec(_bundle(tmp_path))))
    assert (err.code, err.code_message) == (
        protocol.ERR_UNSUPPORTED,
        "CTranslate2 sees no CUDA device",
    )
    assert engine.constructed == []


def test_the_cpu_device_needs_no_cuda_device(engine: _Engine, tmp_path: Path) -> None:
    engine.cuda_devices = 0
    backend = _loaded(tmp_path, device="cpu", compute_type="int8_float32")
    assert engine.constructed[0]["device"] == "cpu"
    result = read_result_json(backend.infer(_request()))
    assert result["runtime"]["device"] == "cpu"
    assert result["runtime"]["compute_type"] == "int8_float32"


def test_a_compute_type_the_device_lacks_is_unsupported(engine: _Engine, tmp_path: Path) -> None:
    err = _refusal(
        lambda: FasterWhisperBackend().load(_spec(_bundle(tmp_path, compute_type="int16")))
    )
    assert (err.code, err.code_message) == (
        protocol.ERR_UNSUPPORTED,
        "the device does not support the entry's compute_type",
    )
    assert engine.constructed == []


def test_compute_types_that_cannot_be_listed_fail_the_load(engine: _Engine, tmp_path: Path) -> None:
    engine.listing_error = RuntimeError("CUDA failed with error unknown error")
    err = _refusal(lambda: FasterWhisperBackend().load(_spec(_bundle(tmp_path))))
    assert (err.code, err.code_message) == (
        protocol.ERR_LOAD_FAILED,
        "the device's compute types could not be listed",
    )
    assert engine.constructed == []


def test_a_changed_model_file_fails_its_digest(engine: _Engine, tmp_path: Path) -> None:
    entry_path = _bundle(tmp_path)
    (entry_path.parent / "model" / "model.bin").write_bytes(b"changed")
    err = _refusal(lambda: FasterWhisperBackend().load(_spec(entry_path)))
    assert (err.code, err.code_message) == (
        protocol.ERR_LOAD_FAILED,
        "artifact_set item 1 does not match its digest",
    )
    assert engine.constructed == []


def test_an_unlisted_file_in_the_model_directory_is_refused(
    engine: _Engine, tmp_path: Path
) -> None:
    entry_path = _bundle(tmp_path)
    (entry_path.parent / "model" / "vocabulary.txt").write_text("unlisted", encoding="utf-8")
    err = _refusal(lambda: FasterWhisperBackend().load(_spec(entry_path)))
    assert (err.code, err.code_message) == (
        protocol.ERR_LOAD_FAILED,
        "an artifact_set directory holds a file its entry does not list",
    )
    assert engine.constructed == []


def test_the_model_directory_must_list_its_tokenizer(engine: _Engine, tmp_path: Path) -> None:
    entry_path = _bundle(tmp_path)
    entry = json.loads(entry_path.read_text(encoding="utf-8"))
    entry["artifact_set"] = [
        item for item in entry["artifact_set"] if item["path"] != "model/tokenizer.json"
    ]
    entry_path.write_text(json.dumps(entry), encoding="utf-8")
    (entry_path.parent / "model" / "tokenizer.json").unlink()
    err = _refusal(lambda: FasterWhisperBackend().load(_spec(entry_path)))
    assert (err.code, err.code_message) == (
        protocol.ERR_CONFIG_INVALID,
        "the model directory's artifact_set must list tokenizer.json",
    )
    assert engine.constructed == []


@pytest.mark.parametrize(
    ("error", "code"),
    [
        (RuntimeError("CUDA failed with error out of memory"), protocol.ERR_OOM_ERROR),
        (
            RuntimeError("cuBLAS failed with status CUBLAS_STATUS_ALLOC_FAILED"),
            protocol.ERR_OOM_ERROR,
        ),
        (MemoryError(), protocol.ERR_OOM_ERROR),
        (RuntimeError("Unable to open file 'model.bin' in model 'x'"), protocol.ERR_LOAD_FAILED),
        (ValueError("Requested float16 compute type, but ..."), protocol.ERR_LOAD_FAILED),
        (OSError("out of memory"), protocol.ERR_LOAD_FAILED),
    ],
)
def test_a_model_that_cannot_be_built_fails_by_class_and_message(
    engine: _Engine, tmp_path: Path, error: BaseException, code: str
) -> None:
    engine.load_error = error
    err = _refusal(lambda: FasterWhisperBackend().load(_spec(_bundle(tmp_path))))
    assert (err.code, err.code_message) == (code, "the Whisper model could not be loaded")


def test_a_model_that_cannot_be_inspected_fails_the_load_and_is_released(
    engine: _Engine, tmp_path: Path
) -> None:
    engine.tokenizer_error = RuntimeError("tokenizer failed")
    err = _refusal(lambda: FasterWhisperBackend().load(_spec(_bundle(tmp_path))))
    assert (err.code, err.code_message) == (
        protocol.ERR_LOAD_FAILED,
        "the loaded model could not be inspected",
    )
    assert engine.released == 1


def test_another_loaded_compute_type_fails_the_load_and_releases_the_model(
    engine: _Engine, tmp_path: Path
) -> None:
    engine.loaded_compute_type = "int8_float16"
    err = _refusal(lambda: FasterWhisperBackend().load(_spec(_bundle(tmp_path))))
    assert (err.code, err.code_message) == (
        protocol.ERR_LOAD_FAILED,
        "CTranslate2 loaded the model with another compute type",
    )
    assert engine.released == 1


def test_a_rate_the_model_does_not_take_is_refused(engine: _Engine, tmp_path: Path) -> None:
    engine.sampling_rate = 8000
    err = _refusal(lambda: FasterWhisperBackend().load(_spec(_bundle(tmp_path))))
    assert (err.code, err.code_message) == (
        protocol.ERR_CONFIG_INVALID,
        "the entry's sample_rate_hz is not the model's input rate",
    )
    assert engine.released == 1


@pytest.mark.parametrize(
    ("multilingual", "tokens", "languages"),
    [
        (False, {"<|en|>", "<|ar|>"}, ["en", "ar"]),
        (True, {"<|en|>"}, ["en", "ar"]),
        (True, {"<|en|>", "<|ar|>", "<|xx|>"}, ["en", "xx"]),
    ],
)
def test_a_language_the_model_does_not_serve_is_refused(
    engine: _Engine,
    tmp_path: Path,
    multilingual: bool,
    tokens: set[str],
    languages: list[str],
) -> None:
    engine.multilingual = multilingual
    engine.tokens = tokens
    err = _refusal(
        lambda: FasterWhisperBackend().load(_spec(_bundle(tmp_path, languages=languages)))
    )
    assert (err.code, err.code_message) == (
        protocol.ERR_CONFIG_INVALID,
        "the entry declares a language the model's tokenizer does not",
    )
    assert engine.released == 1


def test_an_english_only_model_serves_an_entry_that_declares_only_english(
    engine: _Engine, tmp_path: Path
) -> None:
    engine.multilingual = False
    backend = _loaded(tmp_path, languages=["en"])
    assert read_result_json(backend.infer(_request("en")))["language"] == "en"


@pytest.mark.parametrize("window", [0, -1, "16", 16.0, None])
def test_a_model_without_a_readable_input_window_fails_the_load(
    engine: _Engine, tmp_path: Path, window: Any
) -> None:
    engine.n_samples = window
    err = _refusal(lambda: FasterWhisperBackend().load(_spec(_bundle(tmp_path))))
    assert (err.code, err.code_message) == (
        protocol.ERR_LOAD_FAILED,
        "the model's input window could not be read",
    )
    assert engine.released == 1


# ----------------------------------------------------------------------
# infer
# ----------------------------------------------------------------------


def test_infer_returns_the_whole_transcript_as_result_json(engine: _Engine, tmp_path: Path) -> None:
    backend = _loaded(tmp_path)
    outputs = backend.infer(_request("ar", [0, 16384, -32768, 32767, *_THREE_SECONDS[4:]]))
    assert [output.name for output in outputs] == ["result_json"]
    result = read_result_json(outputs)

    ((samples, options),) = engine.transcribed
    assert samples[:4] == [0.0, 0.5, -1.0, 32767 / 32768]
    assert len(samples) == 48_000
    assert options == {"language": "ar", **DECODE_OPTIONS}
    assert DECODE_OPTIONS == {
        "beam_size": 1,
        "temperature": 0.0,
        "condition_on_previous_text": False,
        "vad_filter": False,
        "word_timestamps": True,
    }
    assert engine.exhausted
    assert {key: value for key, value in result.items() if key != "timings_us"} == {
        "language": "ar",
        "text": " Hello there. Bye.",
        "segments": [
            {
                "start_us": 0,
                "end_us": 1_100_000,
                "text": " Hello there.",
                "tokens": [50365, 2425, 456, 13],
                "words": [
                    {"start_us": 0, "end_us": 520_000, "text": " Hello", "probability": 0.75},
                    {
                        "start_us": 520_000,
                        "end_us": 1_100_000,
                        "text": " there.",
                        "probability": 0.5,
                    },
                ],
            },
            {
                "start_us": 1_100_000,
                "end_us": 2_675_000,
                "text": " Bye.",
                "tokens": [4621, 13],
                "words": [
                    {
                        "start_us": 1_290_000,
                        "end_us": 2_675_000,
                        "text": " Bye.",
                        "probability": 0.25,
                    }
                ],
            },
        ],
        "sample_rate_hz": 16000,
        "sample_count": 48_000,
        "decode_options": dict(DECODE_OPTIONS),
        "runtime": {
            "device": "cuda",
            "compute_type": "float16",
            "faster_whisper": "1.2.1",
            "ctranslate2": "4.8.2",
        },
    }
    assert sorted(result["timings_us"]) == ["artifact_verify", "decode", "load", "model_build"]


def test_timings_are_whole_microseconds_of_each_span(
    engine: _Engine, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    ticks = iter(
        [
            1_000_000,  # load starts, before the entry is read
            3_000_500,  # digest verification starts
            7_000_499,  # the tokenizer check is done
            19_000_999,  # the model is built
            20_500_000,  # the model is inspected
            31_000_000,  # decode starts
            33_654_999,  # the last segment is consumed
        ]
    )
    monkeypatch.setattr(
        faster_whisper_module, "time", SimpleNamespace(monotonic_ns=lambda: next(ticks))
    )
    result = read_result_json(_loaded(tmp_path).infer(_request()))
    assert result["timings_us"] == {
        "load": 19_500,
        "artifact_verify": 3_999,
        "model_build": 12_000,
        "decode": 2_654,
    }


def test_the_rate_is_the_entrys_once_the_model_agrees(engine: _Engine, tmp_path: Path) -> None:
    engine.sampling_rate = 8000
    result = read_result_json(_loaded(tmp_path, sample_rate_hz=8000).infer(_request()))
    assert result["sample_rate_hz"] == 8000


def test_a_segment_without_words_has_an_empty_word_list(engine: _Engine, tmp_path: Path) -> None:
    engine.segments = [_Segment(0.0, 0.5, "", [], None)]
    result = read_result_json(_loaded(tmp_path).infer(_request()))
    assert result["text"] == ""
    assert result["segments"] == [
        {"start_us": 0, "end_us": 500_000, "text": "", "tokens": [], "words": []}
    ]


def test_silence_returns_an_empty_transcript(engine: _Engine, tmp_path: Path) -> None:
    engine.segments = []
    result = read_result_json(_loaded(tmp_path).infer(_request()))
    assert (result["text"], result["segments"]) == ("", [])
    assert engine.exhausted


def _clip_of(tmp_path: Path, engine: _Engine, *, rate: int, samples: int) -> dict[str, Any]:
    """Transcribe a clip of ``samples`` at ``rate`` with the engine's segments."""
    engine.sampling_rate = rate
    engine.n_samples = max(samples, 1)
    backend = _loaded(tmp_path, sample_rate_hz=rate)
    return read_result_json(backend.infer(_request("ar", [0] * samples)))


@pytest.mark.parametrize(
    ("seconds", "micros"),
    [
        (0, 0),
        (0.29, 290_000),
        (1.1, 1_100_000),
        (2.01, 2_010_000),
        (2.675, 2_675_000),
        (4.1, 4_100_000),
        (29.99, 29_990_000),
    ],
)
def test_times_become_integer_microseconds(
    engine: _Engine, tmp_path: Path, seconds: float, micros: int
) -> None:
    engine.segments = [_Segment(seconds, seconds, " a", [1], [_Word(seconds, seconds, " a", 1.0)])]
    (segment,) = _clip_of(tmp_path, engine, rate=100, samples=3000)["segments"]
    assert (segment["start_us"], segment["end_us"]) == (micros, micros)
    assert (segment["words"][0]["start_us"], segment["words"][0]["end_us"]) == (micros, micros)


def test_an_interval_that_runs_past_the_clip_ends_with_it(engine: _Engine, tmp_path: Path) -> None:
    engine.segments = [_Segment(0.5, 1.2, " a", [1], [_Word(0.5, 1.2, " a", 1.0)])]
    (segment,) = _clip_of(tmp_path, engine, rate=100, samples=100)["segments"]
    assert (segment["start_us"], segment["end_us"]) == (500_000, 1_000_000)
    assert (segment["words"][0]["start_us"], segment["words"][0]["end_us"]) == (500_000, 1_000_000)


def test_the_clip_length_is_rounded_up_to_a_whole_microsecond(
    engine: _Engine, tmp_path: Path
) -> None:
    # One sample at 3 Hz lasts 333,333.3 microseconds: the clip ends at 333,334.
    engine.segments = [_Segment(0.333334, 0.4, " a", [1], [])]
    (segment,) = _clip_of(tmp_path, engine, rate=3, samples=1)["segments"]
    assert (segment["start_us"], segment["end_us"]) == (333_334, 333_334)
    engine.segments = [_Segment(0.333335, 0.4, " a", [1], [])]
    err = _refusal(lambda: _clip_of(tmp_path / "again", engine, rate=3, samples=1))
    assert err.code_message == "the model returned an interval that starts after the audio"


@pytest.mark.parametrize(
    ("start", "end", "message"),
    [
        (1.01, 1.2, "the model returned an interval that starts after the audio"),
        (0.6, 0.5, "the model returned an interval that ends before it starts"),
    ],
)
@pytest.mark.parametrize("where", ["segment", "word"])
def test_an_interval_outside_the_clip_fails_the_request(
    engine: _Engine, tmp_path: Path, start: float, end: float, message: str, where: str
) -> None:
    word = _Word(start, end, " a", 1.0) if where == "word" else _Word(0.0, 0.5, " a", 1.0)
    bounds = (start, end) if where == "segment" else (0.0, 0.5)
    engine.segments = [_Segment(*bounds, " a", [1], [word])]
    err = _refusal(lambda: _clip_of(tmp_path, engine, rate=100, samples=100))
    assert (err.code, err.code_message) == (protocol.ERR_INFERENCE_FAILED, message)


@pytest.mark.parametrize("bad", [float("nan"), float("inf"), -0.01, True, "1.0", None])
@pytest.mark.parametrize("where", ["segment", "word"])
def test_a_time_that_is_not_a_non_negative_number_fails_the_request(
    engine: _Engine, tmp_path: Path, bad: Any, where: str
) -> None:
    word = _Word(bad if where == "word" else 0.0, 0.5, " a", 1.0)
    engine.segments = [_Segment(bad if where == "segment" else 0.0, 0.5, " a", [1], [word])]
    err = _refusal(lambda: _clip_of(tmp_path, engine, rate=100, samples=100))
    assert (err.code, err.code_message) == (
        protocol.ERR_INFERENCE_FAILED,
        "the model returned a time that is not a non-negative number",
    )


def _shape_cases() -> list[Any]:
    audio = _audio([1, 2])
    text = text_utf8_tensor("ar")
    return [
        pytest.param([text], id="no audio"),
        pytest.param([audio], id="no language"),
        pytest.param([audio, text, NamedTensor("extra", {}, b"")], id="extra tensor"),
        pytest.param([audio, audio], id="two audio tensors"),
        pytest.param([audio, text_utf8_tensor("ar"), text], id="two language tensors"),
    ]


@pytest.mark.parametrize("inputs", _shape_cases())
def test_a_request_needs_exactly_one_audio_and_one_language_tensor(
    engine: _Engine, tmp_path: Path, inputs: list[NamedTensor]
) -> None:
    err = _refusal(lambda: _loaded(tmp_path).infer(inputs))
    assert (err.code, err.code_message) == (
        protocol.ERR_SHAPE_MISMATCH,
        "a request carries exactly one audio_frames and one text_utf8 tensor",
    )
    assert engine.transcribed == []


@pytest.mark.parametrize(
    "audio",
    [
        pytest.param(_audio([1, 2], dtype="float32"), id="float32"),
        pytest.param(_audio([1, 2], dtype="uint8"), id="uint8"),
        pytest.param(_audio([1, 2], shape=[1, 2]), id="two dimensions"),
        pytest.param(_audio([1, 2], shape=[2, 1]), id="two dimensions, first matching"),
        pytest.param(_audio([1, 2], shape=[3]), id="shape longer than payload"),
        pytest.param(_audio([1, 2], shape=[1]), id="shape shorter than payload"),
        pytest.param(_audio([1, 2], shape=[2.0]), id="float extent"),
        pytest.param(_audio([1, 2], shape=[True]), id="bool extent"),
        pytest.param(_audio([1, 2], shape=2), id="scalar shape"),
        pytest.param(
            NamedTensor(AUDIO_FRAMES, {"dtype": "int16", "shape": [1]}, b"\x01\x00\x02"),
            id="odd payload",
        ),
    ],
)
def test_audio_that_is_not_one_dimensional_int16_is_a_shape_mismatch(
    engine: _Engine, tmp_path: Path, audio: NamedTensor
) -> None:
    err = _refusal(lambda: _loaded(tmp_path).infer([audio, text_utf8_tensor("ar")]))
    assert (err.code, err.code_message) == (
        protocol.ERR_SHAPE_MISMATCH,
        "audio_frames must be a one-dimensional int16 tensor of its payload",
    )
    assert engine.transcribed == []


@pytest.mark.parametrize("count", [0, 17])
def test_audio_outside_one_input_window_is_a_shape_mismatch(
    engine: _Engine, tmp_path: Path, count: int
) -> None:
    engine.n_samples = 16
    err = _refusal(lambda: _loaded(tmp_path).infer(_request("ar", [7] * count)))
    assert (err.code, err.code_message) == (
        protocol.ERR_SHAPE_MISMATCH,
        "audio_frames must hold one sample to one model input window",
    )
    assert engine.transcribed == []


@pytest.mark.parametrize("count", [1, 16])
def test_audio_up_to_one_input_window_is_transcribed(
    engine: _Engine, tmp_path: Path, count: int
) -> None:
    engine.n_samples = 16
    engine.segments = []
    result = read_result_json(_loaded(tmp_path).infer(_request("ar", [7] * count)))
    assert result["sample_count"] == count
    assert len(engine.transcribed[0][0]) == count


@pytest.mark.parametrize("language", ["fr", "EN", "en ", "yue"])
def test_a_language_the_entry_does_not_declare_is_unsupported(
    engine: _Engine, tmp_path: Path, language: str
) -> None:
    err = _refusal(lambda: _loaded(tmp_path).infer(_request(language)))
    assert (err.code, err.code_message) == (
        protocol.ERR_UNSUPPORTED,
        "the request names a language its entry does not declare",
    )
    assert engine.transcribed == []


def test_a_language_tag_that_is_not_utf8_is_config_invalid(engine: _Engine, tmp_path: Path) -> None:
    bad = NamedTensor(TEXT_UTF8, {"dtype": "uint8", "shape": [1]}, b"\xff")
    err = _refusal(lambda: _loaded(tmp_path).infer([_audio([1]), bad]))
    assert (err.code, err.code_message) == (
        protocol.ERR_CONFIG_INVALID,
        "`text_utf8` is not valid UTF-8",
    )


@pytest.mark.parametrize(
    ("stage", "error", "code"),
    [
        ("call", RuntimeError("CUDA failed with error out of memory"), protocol.ERR_OOM_ERROR),
        ("call", ValueError("bad audio"), protocol.ERR_INFERENCE_FAILED),
        ("iteration", RuntimeError("CUDA failed with error out of memory"), protocol.ERR_OOM_ERROR),
        ("iteration", MemoryError(), protocol.ERR_OOM_ERROR),
        ("iteration", RuntimeError("decoder failed"), protocol.ERR_INFERENCE_FAILED),
    ],
)
def test_a_failed_transcription_is_typed_by_class_and_message(
    engine: _Engine, tmp_path: Path, stage: str, error: BaseException, code: str
) -> None:
    if stage == "call":
        engine.transcribe_error = error
    else:
        engine.iteration_error = error
    err = _refusal(lambda: _loaded(tmp_path).infer(_request()))
    assert (err.code, err.code_message) == (code, "the audio could not be transcribed")


def test_prime_and_infer_need_a_loaded_model(engine: _Engine) -> None:
    backend = FasterWhisperBackend()
    for action in (backend.prime, lambda: backend.infer(_request())):
        err = _refusal(action)
        assert (err.code, err.code_message) == (
            protocol.ERR_NOT_READY,
            "the faster_whisper backend is not loaded",
        )


def test_infer_async_transcribes_like_infer(engine: _Engine, tmp_path: Path) -> None:
    backend = _loaded(tmp_path)
    result = read_result_json(backend.infer_async(_request()))
    assert result["text"] == " Hello there. Bye."


# ----------------------------------------------------------------------
# unload
# ----------------------------------------------------------------------


def test_unload_releases_the_model_once(engine: _Engine, tmp_path: Path) -> None:
    backend = _loaded(tmp_path)
    backend.unload()
    backend.unload()
    assert engine.released == 1
    err = _refusal(lambda: backend.infer(_request()))
    assert err.code == protocol.ERR_NOT_READY


def test_a_failed_release_is_logged_by_class_and_unload_completes(
    engine: _Engine, tmp_path: Path
) -> None:
    backend = _loaded(tmp_path)
    engine.release_error = RuntimeError(CANARY)
    with _edge_log() as log:
        backend.unload()
    assert log.getvalue().startswith(
        "WARNING:tensorplate.sidecar:faster_whisper model release failed: RuntimeError at "
    )
    assert CANARY not in log.getvalue()
    assert _refusal(lambda: backend.infer(_request())).code == protocol.ERR_NOT_READY


# ----------------------------------------------------------------------
# canaries on the runner's surfaces: frame bytes, health, log
# ----------------------------------------------------------------------


def test_a_load_error_naming_a_canary_path_reaches_no_surface(
    engine: _Engine, sidecar: socket.socket, tmp_path: Path
) -> None:
    entry_path = _canary_bundle(tmp_path)
    engine.load_error = RuntimeError(f"Unable to open file '{entry_path.parent}/model/model.bin'")
    written, response, log = _surfaces(sidecar, [_load_frame(entry_path)])
    assert response.header["error"]["code"] == protocol.ERR_LOAD_FAILED
    assert _CANARY_BYTES not in written
    assert CANARY not in log


def test_a_digest_mismatch_under_a_canary_path_reaches_no_surface(
    engine: _Engine, sidecar: socket.socket, tmp_path: Path
) -> None:
    entry_path = _canary_bundle(tmp_path)
    (entry_path.parent / "model" / "config.json").write_text(CANARY, encoding="utf-8")
    written, response, log = _surfaces(sidecar, [_load_frame(entry_path)])
    assert response.header["error"]["code"] == protocol.ERR_LOAD_FAILED
    assert _CANARY_BYTES not in written
    assert CANARY not in log


@pytest.mark.parametrize(
    ("stage", "message", "released"),
    [
        ("listing", "the device's compute types could not be listed", 0),
        ("inspection", "the loaded model could not be inspected", 1),
    ],
)
def test_a_load_step_error_carrying_a_canary_reaches_no_surface(
    engine: _Engine,
    sidecar: socket.socket,
    tmp_path: Path,
    stage: str,
    message: str,
    released: int,
) -> None:
    error = RuntimeError(f"CUDA failed with error {CANARY}")
    if stage == "listing":
        engine.listing_error = error
    else:
        engine.tokenizer_error = error
    written, response, log = _surfaces(sidecar, [_load_frame(_canary_bundle(tmp_path))])
    assert response.header["error"] == {
        "schema_version": protocol.SCHEMA_VERSION,
        "code": protocol.ERR_LOAD_FAILED,
        "message": message,
    }
    assert engine.released == released
    assert _CANARY_BYTES not in written
    assert CANARY not in log


def test_a_canary_language_in_the_entry_reaches_no_surface(
    engine: _Engine, sidecar: socket.socket, tmp_path: Path
) -> None:
    entry_path = _canary_bundle(tmp_path, languages=["en", CANARY])
    written, response, log = _surfaces(sidecar, [_load_frame(entry_path)])
    assert response.header["error"]["code"] == protocol.ERR_CONFIG_INVALID
    assert _CANARY_BYTES not in written
    assert CANARY not in log


def test_a_canary_language_in_the_request_reaches_no_surface(
    engine: _Engine, sidecar: socket.socket, tmp_path: Path
) -> None:
    frames = [
        _load_frame(_canary_bundle(tmp_path)),
        _frame(protocol.KIND_PRIME),
        _infer_frame(_request(CANARY)),
    ]
    written, response, log = _surfaces(sidecar, frames)
    assert response.header["error"]["code"] == protocol.ERR_UNSUPPORTED
    assert _CANARY_BYTES not in written
    assert CANARY not in log


@pytest.mark.parametrize("stage", ["call", "iteration"])
def test_a_transcription_error_carrying_a_canary_reaches_no_surface(
    engine: _Engine, sidecar: socket.socket, tmp_path: Path, stage: str
) -> None:
    error = RuntimeError(f"decode of {CANARY} failed: CUDA failed with error out of memory")
    if stage == "call":
        engine.transcribe_error = error
    else:
        engine.iteration_error = error
    frames = [
        _load_frame(_canary_bundle(tmp_path)),
        _frame(protocol.KIND_PRIME),
        _infer_frame(_request()),
    ]
    written, response, log = _surfaces(sidecar, frames)
    assert response.header["error"] == {
        "schema_version": protocol.SCHEMA_VERSION,
        "code": protocol.ERR_OOM_ERROR,
        "message": "the audio could not be transcribed",
    }
    assert _CANARY_BYTES not in written
    assert CANARY not in log


def test_a_transcript_reaches_the_result_and_no_other_surface(
    engine: _Engine, sidecar: socket.socket, tmp_path: Path
) -> None:
    engine.segments = [_Segment(0.0, 1.0, f" {CANARY}", [1], [_Word(0.0, 1.0, f" {CANARY}", 1.0)])]
    frames = [
        _load_frame(_canary_bundle(tmp_path)),
        _frame(protocol.KIND_PRIME),
        _infer_frame(_request()),
    ]
    written, response, log = _surfaces(sidecar, frames)
    assert response.header["status"] == protocol.STATUS_OK
    (tensor,) = response.header["tensors"]
    start = tensor["payload_offset"]
    payload = response.payload[start : start + tensor["payload_length"]]
    result = read_result_json([NamedTensor(tensor["name"], tensor["tensor"], payload)])
    assert result["text"] == f" {CANARY}"
    assert written.count(_CANARY_BYTES) == payload.count(_CANARY_BYTES) == 3
    assert "withheld a WARNING record from faster_whisper" in log
    assert CANARY not in log


# ----------------------------------------------------------------------
# jobs
# ----------------------------------------------------------------------

_TOKENS = [50365, 2425, 456, 13, 4621, 13]


def _job(language: str = "ar", samples: list[int] | None = None) -> JobRequest:
    payload = _audio(_THREE_SECONDS if samples is None else samples).payload
    return JobRequest(JobIdentity(1, 1, 1), protocol.JOB_CLASS_STT_DECODE, 0, payload, language)


def _job_peer(connect: Callable[..., Peer], entry_path: Path) -> tuple[Peer, dict[str, Any]]:
    """A live runner, and its answer to a load of ``entry_path`` that enables jobs."""
    peer = connect()
    load = _load_frame(entry_path).header
    return peer, exchange(peer, {**load, "capabilities": [protocol.CAPABILITY_SPEECH_JOBS_V1]})


def test_stt_decode_is_declared_for_a_loaded_model_at_the_job_input_rate(
    engine: _Engine, tmp_path: Path
) -> None:
    backend = FasterWhisperBackend()
    assert backend.job_classes() == ()
    backend.load(_spec(_bundle(tmp_path)))
    assert backend.job_classes() == (protocol.JOB_CLASS_STT_DECODE,)
    engine.sampling_rate = 8000
    assert _loaded(tmp_path, under="other", sample_rate_hz=8000).job_classes() == ()


def test_a_job_is_permitted_in_a_declared_language_within_one_input_window(
    engine: _Engine, tmp_path: Path
) -> None:
    engine.n_samples = 16
    backend = _loaded(tmp_path)
    assert backend.permits_job(_job("ar", [7] * 16))
    assert backend.permits_job(_job("en", [7]))
    assert not backend.permits_job(_job("fr", [7]))
    assert not backend.permits_job(_job("ar", [7] * 17))
    synthesis = dataclasses.replace(_job("ar", [7]), job_class=protocol.JOB_CLASS_TTS_SYNTHESIS)
    assert not backend.permits_job(synthesis)
    backend.unload()
    assert not backend.permits_job(_job("ar", [7]))


def test_run_job_returns_the_text_and_tokens_of_the_decode_infer_runs(
    engine: _Engine, tmp_path: Path
) -> None:
    backend = _loaded(tmp_path)
    samples = [0, 16384, -32768, 32767, *_THREE_SECONDS[4:]]
    result = backend.run_job(_job("ar", samples))
    assert result == TranscriptResult(" Hello there. Bye.", _TOKENS)
    assert result.text == read_result_json(backend.infer(_request("ar", samples)))["text"]
    job_decode, infer_decode = engine.transcribed
    assert job_decode == infer_decode
    assert job_decode[0][:4] == [0.0, 0.5, -1.0, 32767 / 32768]
    assert job_decode[1] == {"language": "ar", **DECODE_OPTIONS}
    assert _refusal(lambda: FasterWhisperBackend().run_job(_job())).code == protocol.ERR_NOT_READY


def test_a_live_runner_declares_stt_decode_and_runs_a_permitted_job(
    engine: _Engine, connect: Callable[..., Peer], tmp_path: Path
) -> None:
    engine.segments = engine.segments[:1]  # the golden submit is one second long
    peer, loaded = _job_peer(connect, _bundle(tmp_path))
    assert (loaded["status"], loaded["job_classes"]) == (protocol.STATUS_OK, ["stt_decode"])
    peer.send(submit(1))
    frames = [peer.read().header for _ in range(3)]
    assert [digest(frame) for frame in frames] == [(ACCEPTED, 1), *ended(1)]
    assert frames[1]["result"] == {
        "type": "transcript",
        "text": " Hello there.",
        "tokens": _TOKENS[:4],
        "words": [],
    }
    ((decoded, options),) = engine.transcribed
    assert (len(decoded), options) == (16_000, {"language": "en", **DECODE_OPTIONS})
    peer.send(submit(2, options={"language": "fr"}))
    assert read(peer, 2) == ended(2, FAILED, "unsupported", "job_not_permitted")
    # A model at another rate declares no class, so its job-enabled load is refused.
    engine.sampling_rate = 8000
    _, refused = _job_peer(connect, _bundle(tmp_path, under="other", sample_rate_hz=8000))
    assert (refused["error"]["code"], engine.released) == (protocol.ERR_UNSUPPORTED, 1)


def test_a_failed_job_whose_error_carries_a_canary_reaches_no_surface(
    engine: _Engine, connect: Callable[..., Peer], tmp_path: Path
) -> None:
    engine.iteration_error = RuntimeError(f"decode of {CANARY} failed")
    peer, loaded = _job_peer(connect, _canary_bundle(tmp_path))
    with _edge_log() as log:
        peer.send(submit(1))
        frames = [loaded, *(peer.read().header for _ in range(3))]
        frames.append(exchange(peer, message(protocol.KIND_HEALTH_CHECK)))
    assert [digest(frame) for frame in frames[1:4]] == [
        (ACCEPTED, 1),
        *ended(1, FAILED, "inference_failed"),
    ]
    assert frames[2]["error"]["message"] == "the audio could not be transcribed"
    assert frames[4]["health"]["last_error"] == "the audio could not be transcribed"
    assert "withheld a WARNING record from faster_whisper" in log.getvalue()
    assert CANARY not in json.dumps(frames) + log.getvalue()
