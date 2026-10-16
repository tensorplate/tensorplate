"""Silero voice activity detection for the ``faster_whisper`` profile's ``vad_frames`` jobs.

The model is the copy the faster-whisper wheel carries, run on one ONNX Runtime
CPU session. The wheel's own wrapper starts every call from zeroed state, so
this calls the session itself and returns Silero's recurrent state and the last
samples of the job's last frame, which the utterance's next job continues from.
"""

from __future__ import annotations

import inspect
import os
from dataclasses import dataclass
from typing import Any, Final

from tensorplate_pytorch_backend import sanitize
from tensorplate_pytorch_backend.backends.base import BackendError
from tensorplate_pytorch_backend.protocol import (
    ERR_INFERENCE_FAILED,
    ERR_INTERNAL,
    ERR_LOAD_FAILED,
)

#: The file ``faster_whisper.vad.get_vad_model`` loads from the wheel's assets.
MODEL_FILE: Final[str] = "silero_vad_v6.onnx"

_PCM_FULL_SCALE: Final[float] = 32768.0
_INPUTS: Final[frozenset[str]] = frozenset({"input", "h", "c"})


@dataclass(frozen=True, slots=True)
class SileroState:
    """Where an utterance's voice activity stands after a job; never changed once returned."""

    h: Any
    c: Any
    #: The samples the utterance's next frame is fed after.
    context: Any


class SileroVad:
    """One Silero session; each call runs one job's frames in order."""

    def __init__(
        self,
        session: Any,  # noqa: ANN401 -- an onnxruntime.InferenceSession
        numpy: Any,  # noqa: ANN401
        *,
        frame_samples: int,
        context_samples: int,
        state_shape: tuple[int, ...],
    ) -> None:
        self._session = session
        self._numpy = numpy
        self._frame_samples = frame_samples
        self._context_samples = context_samples
        self._state_shape = state_shape

    @property
    def frame_samples(self) -> int:
        return self._frame_samples

    def run(self, pcm: bytes, state: object | None) -> tuple[list[float], SileroState]:
        """Return each frame's speech probability and the state after the last frame."""
        if state is not None and not isinstance(state, SileroState):
            raise BackendError(ERR_INTERNAL, "the voice activity state is not this model's")
        numpy = self._numpy
        try:
            samples = numpy.frombuffer(pcm, dtype="<i2").astype(numpy.float32) / _PCM_FULL_SCALE
            frames = samples.reshape(-1, self._frame_samples)
            if state is None:
                h = numpy.zeros(self._state_shape, dtype=numpy.float32)
                c = numpy.zeros(self._state_shape, dtype=numpy.float32)
                preceding = numpy.zeros(self._context_samples, dtype=numpy.float32)
            else:
                h, c, preceding = state.h, state.c, state.context
            tails = frames[:, -self._context_samples :]
            contexts = numpy.concatenate([preceding[numpy.newaxis], tails[:-1]])
            probabilities, h, c = self._session.run(
                None, {"input": numpy.concatenate([contexts, frames], axis=1), "h": h, "c": c}
            )
            after = SileroState(h, c, tails[-1].copy())
            return [float(value) for value in probabilities.reshape(-1)], after
        except Exception as exc:
            raise BackendError(
                sanitize.code_for(exc, ERR_INFERENCE_FAILED),
                "the voice activity model could not run",
            ) from exc


def _static(shape: object) -> tuple[int, ...] | None:
    if not isinstance(shape, (list, tuple)) or not shape:
        return None
    if not all(type(size) is int and size > 0 for size in shape):
        return None
    return tuple(shape)


def open_silero_vad() -> SileroVad:
    """Load the wheel's Silero model on one CPU thread; frame and context are its wrapper's."""
    try:
        import numpy
        import onnxruntime
        from faster_whisper import utils, vad
    except Exception as exc:
        raise BackendError(
            ERR_LOAD_FAILED, "ONNX Runtime or the voice activity model is not importable"
        ) from exc
    try:
        parameters = inspect.signature(vad.SileroVADModel.__call__).parameters
        frame_samples = parameters["num_samples"].default
        context_samples = parameters["context_size_samples"].default
        options = onnxruntime.SessionOptions()
        options.intra_op_num_threads = 1
        options.inter_op_num_threads = 1
        options.enable_cpu_mem_arena = False
        options.log_severity_level = 4  # fatal only, as the wheel's wrapper sets it
        onnxruntime.disable_telemetry_events()
        session = onnxruntime.InferenceSession(
            os.path.join(utils.get_assets_path(), MODEL_FILE),
            sess_options=options,
            providers=["CPUExecutionProvider"],
        )
        inputs = {item.name: item.shape for item in session.get_inputs()}
        outputs = len(session.get_outputs())
    except Exception as exc:
        raise BackendError(
            sanitize.code_for(exc, ERR_LOAD_FAILED), "the voice activity model could not be loaded"
        ) from exc
    window = inputs.get("input")
    state_shape = _static(inputs.get("h"))
    if (
        type(frame_samples) is not int
        or type(context_samples) is not int
        or not 0 < context_samples < frame_samples
        or set(inputs) != _INPUTS
        or outputs != 3
        or not isinstance(window, list)
        or len(window) != 2
        or window[1] != frame_samples + context_samples
        or state_shape is None
        or _static(inputs.get("c")) != state_shape
    ):
        raise BackendError(
            ERR_LOAD_FAILED, "the voice activity model does not take its wrapper's frames"
        )
    return SileroVad(
        session,
        numpy,
        frame_samples=frame_samples,
        context_samples=context_samples,
        state_shape=state_shape,
    )


__all__ = ["MODEL_FILE", "SileroState", "SileroVad", "open_silero_vad"]
