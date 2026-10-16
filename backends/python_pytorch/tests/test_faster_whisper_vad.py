"""``vad_frames`` on the faster_whisper profile: the wheel's Silero model, its state carried.

The load and permit cases run against the faked engines. The cases that run the
model need faster-whisper's wheel, ONNX Runtime and NumPy, and are skipped
where they are not installed, as on the hosted runners.
"""

from __future__ import annotations

import importlib
import itertools
import os
import socket
import sys
from collections import defaultdict
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest

from tensorplate_pytorch_backend import protocol
from tensorplate_pytorch_backend.backends.faster_whisper import FasterWhisperBackend
from tensorplate_pytorch_backend.backends.silero_vad import MODEL_FILE, SileroVad, open_silero_vad
from tensorplate_pytorch_backend.job_objects import JobIdentity, JobRequest
from test_faster_whisper_backend import (
    _CANARY_BYTES,
    CANARY,
    VAD_ASSETS,
    _bundle,
    _canary_bundle,
    _Engine,
    _job_peer,
    _load_frame,
    _loaded,
    _refusal,
    _spec,
    _surfaces,
)
from test_faster_whisper_backend import engine as engine  # the fixture
from test_faster_whisper_backend import sidecar as sidecar  # the fixture
from test_speech_jobs_golden_replay import Peer
from test_speech_jobs_golden_replay import connect as connect  # the fixture
from test_speech_jobs_runner import ACCEPTED, digest, ended
from test_vad_lane import vad_submit


def _optional(name: str) -> Any:
    try:
        return importlib.import_module(name)
    except Exception:
        return None


# Imported before any case fakes them; None where they are not installed.
_NUMPY = _optional("numpy")
_ONNXRUNTIME = _optional("onnxruntime")
_WHEEL_VAD = _optional("faster_whisper.vad")
_WHEEL_UTILS = _optional("faster_whisper.utils")

needs_numpy = pytest.mark.skipif(_NUMPY is None, reason="needs NumPy")
needs_silero = pytest.mark.skipif(
    any(module is None for module in (_NUMPY, _ONNXRUNTIME, _WHEEL_VAD, _WHEEL_UTILS)),
    reason="needs faster-whisper's wheel, ONNX Runtime and NumPy",
)

_FRAME = 512


def _vad_job(frame_samples: int = _FRAME, frames: int = 1) -> JobRequest:
    return JobRequest(
        JobIdentity(1, 1, 1),
        protocol.JOB_CLASS_VAD_FRAMES,
        0,
        bytes(2 * frame_samples * frames),
        frame_samples=frame_samples,
        frame_count=frames,
        utterance_id=1,
    )


# ----------------------------------------------------------------------
# load and permit, with ONNX Runtime faked
# ----------------------------------------------------------------------


def test_a_load_opens_the_wheels_silero_model_on_one_cpu_thread_with_telemetry_off(
    engine: _Engine, tmp_path: Path
) -> None:
    _loaded(tmp_path)
    ((path, options, providers),) = engine.vad_sessions
    assert path == f"{VAD_ASSETS}/silero_vad_v6.onnx"
    assert providers == ["CPUExecutionProvider"]
    assert vars(options) == {
        "intra_op_num_threads": 1,
        "inter_op_num_threads": 1,
        "enable_cpu_mem_arena": False,
        "log_severity_level": 4,
    }
    assert engine.vad_events == ["telemetry_off", "session"]


def test_vad_frames_are_permitted_in_the_wrappers_frame_size_only(
    engine: _Engine, tmp_path: Path
) -> None:
    backend = _loaded(tmp_path)
    assert backend.permits_job(_vad_job(frames=32))
    assert not backend.permits_job(_vad_job(frame_samples=256))
    engine.vad_frame, engine.vad_context = 256, 32
    engine.vad_inputs["input"] = ["seq_len", 288]
    other = _loaded(tmp_path / "other")
    assert other.permits_job(_vad_job(frame_samples=256))
    assert not other.permits_job(_vad_job())
    other.unload()
    assert not other.permits_job(_vad_job(frame_samples=256))


def _without_onnxruntime(engine: _Engine, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setitem(sys.modules, "onnxruntime", None)


def _failing_session(engine: _Engine, monkeypatch: pytest.MonkeyPatch) -> None:
    engine.vad_error = RuntimeError("no model")


def _inputs(**changes: list[Any]) -> Callable[[_Engine, pytest.MonkeyPatch], None]:
    def change(engine: _Engine, monkeypatch: pytest.MonkeyPatch) -> None:
        engine.vad_inputs.update(changes)

    return change


def _wrapper(frame: Any, context: Any) -> Callable[[_Engine, pytest.MonkeyPatch], None]:
    def change(engine: _Engine, monkeypatch: pytest.MonkeyPatch) -> None:
        engine.vad_frame, engine.vad_context = frame, context

    return change


def _two_outputs(engine: _Engine, monkeypatch: pytest.MonkeyPatch) -> None:
    engine.vad_outputs = 2


def _context_as_long_as_the_frame(engine: _Engine, monkeypatch: pytest.MonkeyPatch) -> None:
    engine.vad_frame, engine.vad_context = 64, 64
    engine.vad_inputs["input"] = ["seq_len", 128]


_UNIMPORTABLE = "ONNX Runtime or the voice activity model is not importable"
_UNLOADABLE = "the voice activity model could not be loaded"
_MISMATCHED = "the voice activity model does not take its wrapper's frames"


@pytest.mark.parametrize(
    ("change", "message"),
    [
        pytest.param(_without_onnxruntime, _UNIMPORTABLE, id="no-onnxruntime"),
        pytest.param(_failing_session, _UNLOADABLE, id="session-fails"),
        pytest.param(_inputs(input=["seq_len", 577]), _MISMATCHED, id="wider-input"),
        pytest.param(_inputs(input=[576]), _MISMATCHED, id="one-dimensional-input"),
        pytest.param(
            _inputs(h=[1, "batch", 128], c=[1, "batch", 128]), _MISMATCHED, id="dynamic-state"
        ),
        pytest.param(_inputs(h=[1, 0, 128], c=[1, 0, 128]), _MISMATCHED, id="empty-state"),
        pytest.param(_inputs(c=[2, 1, 128]), _MISMATCHED, id="state-shapes-differ"),
        pytest.param(_inputs(sr=[1]), _MISMATCHED, id="another-input"),
        pytest.param(_two_outputs, _MISMATCHED, id="two-outputs"),
        pytest.param(_wrapper(512.0, 64), _MISMATCHED, id="fractional-frame"),
        pytest.param(_wrapper(512, 64.0), _MISMATCHED, id="fractional-context"),
        pytest.param(_wrapper(512, 0), _MISMATCHED, id="no-context"),
        pytest.param(_context_as_long_as_the_frame, _MISMATCHED, id="context-not-shorter"),
    ],
)
def test_a_silero_model_that_cannot_run_the_wrappers_frames_fails_the_load(
    engine: _Engine,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
    change: Callable[[_Engine, pytest.MonkeyPatch], None],
    message: str,
) -> None:
    change(engine, monkeypatch)
    backend = FasterWhisperBackend()
    refused = _refusal(lambda: backend.load(_spec(_bundle(tmp_path))))
    assert (refused.code, refused.code_message) == (protocol.ERR_LOAD_FAILED, message)
    assert engine.released == 1
    assert backend.job_classes() == ()


def test_a_silero_load_out_of_memory_is_oom_error(engine: _Engine, tmp_path: Path) -> None:
    engine.vad_error = MemoryError()
    refused = _refusal(lambda: FasterWhisperBackend().load(_spec(_bundle(tmp_path))))
    assert (refused.code, refused.code_message) == (protocol.ERR_OOM_ERROR, _UNLOADABLE)
    assert engine.released == 1


def test_a_silero_load_error_carrying_a_canary_reaches_no_surface(
    engine: _Engine, sidecar: socket.socket, tmp_path: Path
) -> None:
    engine.vad_error = RuntimeError(f"Load model from /models/{CANARY}/vad.onnx failed")
    written, response, log = _surfaces(sidecar, [_load_frame(_canary_bundle(tmp_path))])
    assert response.header["error"] == {
        "schema_version": protocol.SCHEMA_VERSION,
        "code": protocol.ERR_LOAD_FAILED,
        "message": _UNLOADABLE,
    }
    assert engine.released == 1
    assert _CANARY_BYTES not in written
    assert CANARY not in log


def test_run_vad_needs_a_loaded_model_and_a_state_it_returned(
    engine: _Engine, tmp_path: Path
) -> None:
    unloaded = _refusal(lambda: FasterWhisperBackend().run_vad(_vad_job(), None))
    assert unloaded.code == protocol.ERR_NOT_READY
    backend = _loaded(tmp_path)
    foreign = _refusal(lambda: backend.run_vad(_vad_job(), object()))
    assert (foreign.code, foreign.code_message) == (
        protocol.ERR_INTERNAL,
        "the voice activity state is not this model's",
    )


@needs_numpy
@pytest.mark.parametrize(
    ("error", "code"),
    [
        (RuntimeError(f"input of {CANARY} is invalid"), protocol.ERR_INFERENCE_FAILED),
        (MemoryError(), protocol.ERR_OOM_ERROR),
    ],
)
def test_a_session_that_raises_fails_the_job_by_class_and_message(
    error: BaseException, code: str
) -> None:
    class Raising:
        def run(self, *_: Any) -> Any:
            raise error

    vad = SileroVad(
        Raising(), _NUMPY, frame_samples=_FRAME, context_samples=64, state_shape=(1, 1, 128)
    )
    refused = _refusal(lambda: vad.run(bytes(2 * _FRAME), None))
    assert (refused.code, refused.code_message) == (code, "the voice activity model could not run")
    assert refused.__cause__ is error


@needs_numpy
def test_the_state_keeps_a_copy_of_the_last_frames_tail_not_a_view_of_the_job() -> None:
    class Passing:
        def run(self, _: Any, feeds: dict[str, Any]) -> Any:
            rows = feeds["input"].shape[0]
            return _NUMPY.zeros((rows, 1), dtype=_NUMPY.float32), feeds["h"], feeds["c"]

    vad = SileroVad(
        Passing(), _NUMPY, frame_samples=_FRAME, context_samples=64, state_shape=(2, 1, 128)
    )
    _, state = vad.run(bytes(2 * _FRAME * 3), None)
    # A view would pin the job's whole frame array for as long as the session holds the state.
    assert state.context.base is None
    assert state.context.shape == (64,)


# ----------------------------------------------------------------------
# the real model
# ----------------------------------------------------------------------


def _speech(seed: int, f0: float) -> bytes:
    """Voiced, syllable-paced sound between silences, as whole frames of s16le PCM."""
    numpy = _NUMPY
    rate = 16_000
    t = numpy.arange(3 * rate) / rate
    phase = 2 * numpy.pi * numpy.cumsum(f0 + 20 * numpy.sin(2 * numpy.pi * 0.7 * t)) / rate
    harmonics = sum((1.5 if 4 <= k <= 7 else 1.0) / k * numpy.sin(k * phase) for k in range(1, 25))
    envelope = numpy.clip(numpy.sin(2 * numpy.pi * 3.0 * t), 0, None)
    noise = numpy.random.default_rng(seed).standard_normal(len(t))
    voiced = 0.25 * harmonics / numpy.max(numpy.abs(harmonics)) * envelope + 0.01 * noise
    silence = numpy.zeros(rate // 2)
    audio = numpy.concatenate([silence, voiced, silence, voiced[: len(t) // 2], silence])
    audio = audio[: len(audio) // _FRAME * _FRAME]
    return bytes(numpy.clip(numpy.round(audio * 32767), -32768, 32767).astype("<i2").tobytes())


def _whole(pcm: bytes) -> list[float]:
    """The wheel's own wrapper over the whole utterance."""
    numpy = _NUMPY
    samples = numpy.frombuffer(pcm, dtype="<i2").astype(numpy.float32) / 32768.0
    model = _WHEEL_VAD.SileroVADModel(os.path.join(_WHEEL_UTILS.get_assets_path(), MODEL_FILE))
    # The wrapper zeroes the end of the last frame it is given, so it gets one more,
    # as faster-whisper's get_speech_timestamps pads its audio.
    padded = numpy.concatenate([samples, numpy.zeros(_FRAME, dtype=numpy.float32)])
    whole = [float(value) for value in numpy.asarray(model(padded)).reshape(-1)]
    return whole[: len(samples) // _FRAME]


def _jobs(pcm: bytes, sizes: tuple[int, ...]) -> Iterator[bytes]:
    """The utterance as consecutive jobs of ``sizes`` frames, repeated until it ends."""
    start, turn = 0, 0
    while start < len(pcm):
        end = start + 2 * _FRAME * sizes[turn % len(sizes)]
        yield pcm[start:end]
        start, turn = end, turn + 1


@needs_silero
@pytest.mark.parametrize(
    "sizes", [(1,), (32,), (5, 32, 1, 17, 3)], ids=["single-frames", "full-jobs", "mixed"]
)
def test_jobs_that_carry_the_state_score_each_frame_as_one_call_over_the_utterance(
    sizes: tuple[int, ...],
) -> None:
    pcm = _speech(1, 120.0)
    whole = _whole(pcm)
    assert min(whole) < 0.5 < max(whole)  # the model hears both speech and silence in it
    vad = open_silero_vad()
    scored: list[float] = []
    state = None
    for job in _jobs(pcm, sizes):
        probabilities, state = vad.run(job, state)
        scored += probabilities
    assert scored == whole


@needs_silero
def test_a_job_leaves_the_state_it_continued_from_unchanged() -> None:
    first, second, *_ = _jobs(_speech(1, 120.0), (40,))
    vad = open_silero_vad()
    _, state = vad.run(first, None)
    before = [_NUMPY.copy(part) for part in (state.h, state.c, state.context)]
    once, _ = vad.run(second, state)
    again, _ = vad.run(second, state)
    assert once == again
    assert all(map(_NUMPY.array_equal, before, (state.h, state.c, state.context)))
    assert vad.run(second, None)[0] != once  # the state is what makes the difference


@pytest.fixture
def real_silero(engine: _Engine, monkeypatch: pytest.MonkeyPatch) -> None:
    """The wheel's Silero model, ONNX Runtime and NumPy beside the faked Whisper engine."""
    monkeypatch.setitem(sys.modules, "numpy", _NUMPY)
    monkeypatch.setitem(sys.modules, "onnxruntime", _ONNXRUNTIME)
    monkeypatch.setattr(sys.modules["faster_whisper"], "vad", _WHEEL_VAD)
    monkeypatch.setattr(sys.modules["faster_whisper"], "utils", _WHEEL_UTILS)


@needs_silero
def test_two_sessions_and_two_utterances_keep_their_own_state_in_a_live_runner(
    real_silero: None, connect: Callable[..., Peer], tmp_path: Path
) -> None:
    peer, loaded = _job_peer(connect, _bundle(tmp_path))
    assert loaded["job_classes"] == ["stt_decode", "vad_frames"]
    audio = {(1, 1): _speech(1, 120.0), (2, 1): _speech(2, 180.0), (1, 2): _speech(3, 150.0)}
    one = [((1, 1), pcm) for pcm in _jobs(audio[1, 1], (9,))]
    one += [((1, 2), pcm) for pcm in _jobs(audio[1, 2], (32,))]
    two = [((2, 1), pcm) for pcm in _jobs(audio[2, 1], (13,))]
    # The sessions alternate job by job; session 1 starts its second utterance mid-way.
    plan = [job for pair in itertools.zip_longest(one, two) for job in pair if job is not None]
    scored: defaultdict[tuple[int, int], list[float]] = defaultdict(list)
    for job_id, ((session, utterance), pcm) in enumerate(plan, start=1):
        peer.send(vad_submit(job_id, session, utterance, len(pcm) // (2 * _FRAME)), pcm)
        assert digest(peer.read().header) == (ACCEPTED, job_id)
        completed = peer.read().header
        assert digest(completed) == ended(job_id)[0]
        scored[session, utterance] += completed["result"]["probabilities"]
        assert digest(peer.read().header) == ended(job_id)[1]
    assert dict(scored) == {key: _whole(pcm) for key, pcm in audio.items()}
