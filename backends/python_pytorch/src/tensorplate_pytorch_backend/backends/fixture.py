"""Fixture backend.

Implements the :class:`Backend` interface without any model or third-
party dependency. The fixture backend echoes each input tensor back as
an output named ``"echo_<input_name>"`` with the same metadata and
payload bytes. It is used by V01-E05-F04 / V01-E05-F06 tests to exercise
the runner end-to-end without loading SmolVLA or PyTorch.

Failure-injection hooks
    Set ``fail_load`` / ``fail_prime`` / ``fail_infer`` to a
    ``(code, message)`` tuple before calling the corresponding
    lifecycle method to force a typed sidecar error. Used by the
    failure-injection conformance tests in V01-E05-F06-T03.

Jobs
    It runs ``stt_decode`` and ``tts_synthesis`` jobs with fixed results.
    ``job_languages`` limits the languages it permits (None permits any),
    ``fail_job`` is a ``(code, message)`` tuple like the hooks above,
    ``job_started`` is set when a job starts, and a job waits for
    ``job_gate``, when a test assigned one, before it returns. An integer
    ``job_delay_ms`` in the entry JSON holds every job open that long, for a
    test that drives the sidecar from another process.
"""

from __future__ import annotations

import contextlib
import threading
import time
from collections.abc import Iterator
from typing import Any, Final

from tensorplate_pytorch_backend.backends.base import (
    Backend,
    BackendError,
    NamedTensor,
    RuntimeCapability,
)
from tensorplate_pytorch_backend.configuration import ArtifactConfigError, read_artifact_config
from tensorplate_pytorch_backend.job_objects import (
    AudioChunkResult,
    JobRequest,
    JobResult,
    TranscriptResult,
    WordUnit,
)
from tensorplate_pytorch_backend.protocol import (
    ERR_CONFIG_INVALID,
    ERR_NOT_READY,
    ERR_SHAPE_MISMATCH,
    JOB_CLASS_STT_DECODE,
    JOB_CLASS_TTS_SYNTHESIS,
)

_MAX_JOB_DELAY_MS: Final[int] = 60_000


class FixtureBackend(Backend):
    """No-dependency echo backend used by sidecar contract tests."""

    def __init__(self) -> None:
        self._loaded = False
        self._primed = False
        self.fail_load: tuple[str, str] | None = None
        self.fail_prime: tuple[str, str] | None = None
        self.fail_infer: tuple[str, str] | None = None
        self.cancelled_request_ids: list[str] = []
        self.job_languages: frozenset[str] | None = None
        self.fail_job: tuple[str, str] | None = None
        self.job_started = threading.Event()
        self.job_gate: threading.Event | None = None
        #: Whether a job or an infer ever started while another was running.
        self.overlapped = False
        self._busy = False
        self._job_delay_ms = 0

    @property
    def name(self) -> str:
        return "fixture"

    @property
    def runtime_capability(self) -> RuntimeCapability | None:
        return None

    def load(self, model_spec: dict[str, Any]) -> None:
        if self.fail_load is not None:
            code, msg = self.fail_load
            raise BackendError(code, msg)
        # The fixture has no artifact to load; an entry JSON may set job_delay_ms.
        try:
            delay = read_artifact_config(model_spec).get("job_delay_ms", 0)
        except ArtifactConfigError as exc:
            raise BackendError(ERR_CONFIG_INVALID, str(exc)) from exc
        if type(delay) is not int or not 0 <= delay <= _MAX_JOB_DELAY_MS:
            raise BackendError(ERR_CONFIG_INVALID, "job_delay_ms must be an integer in 0..60000")
        self._job_delay_ms = delay
        self._loaded = True

    def prime(self) -> None:
        if not self._loaded:
            raise BackendError(ERR_NOT_READY, "fixture backend not loaded")
        if self.fail_prime is not None:
            code, msg = self.fail_prime
            raise BackendError(code, msg)
        self._primed = True

    def infer(self, inputs: list[NamedTensor]) -> list[NamedTensor]:
        if not self._primed:
            raise BackendError(ERR_NOT_READY, "fixture backend not primed")
        if self.fail_infer is not None:
            code, msg = self.fail_infer
            raise BackendError(code, msg)
        if not inputs:
            raise BackendError(ERR_SHAPE_MISMATCH, "fixture backend requires at least one input")
        # Echo each input with `"echo_<name>"` output.
        with self._exclusive():
            outputs = [
                NamedTensor(name=f"echo_{inp.name}", tensor=dict(inp.tensor), payload=inp.payload)
                for inp in inputs
            ]
        return outputs

    def infer_async(self, inputs: list[NamedTensor]) -> list[NamedTensor]:
        # Fixture has no real async; reuse the synchronous path. The
        # runner still reports `async_id` correctly.
        return self.infer(inputs)

    def cancel(self, request_id: str) -> None:
        self.cancelled_request_ids.append(request_id)

    def unload(self) -> None:
        self._loaded = False
        self._primed = False

    def job_classes(self) -> tuple[str, ...]:
        return (JOB_CLASS_STT_DECODE, JOB_CLASS_TTS_SYNTHESIS)

    def permits_job(self, request: JobRequest) -> bool:
        return self.job_languages is None or request.language in self.job_languages

    def run_job(self, request: JobRequest) -> JobResult:
        with self._exclusive():
            self.job_started.set()
            if self.job_gate is not None:
                self.job_gate.wait(timeout=_MAX_JOB_DELAY_MS / 1000)
            time.sleep(self._job_delay_ms / 1000)
            if self.fail_job is not None:
                raise BackendError(*self.fail_job)
            if request.job_class == JOB_CLASS_STT_DECODE:
                start = request.start_sample
                word = WordUnit("hello", 0, 1, start, start + len(request.payload) // 4)
                return TranscriptResult("hello", (1,), (word,))
            # 80 samples of silence for each byte of text.
            return AudioChunkResult(bytes(160 * len(request.payload)))

    @contextlib.contextmanager
    def _exclusive(self) -> Iterator[None]:
        self.overlapped = self.overlapped or self._busy
        self._busy = True
        try:
            yield
        finally:
            self._busy = False


__all__ = ["FixtureBackend"]
