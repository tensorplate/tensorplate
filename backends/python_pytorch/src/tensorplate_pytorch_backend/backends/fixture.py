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
"""

from __future__ import annotations

from typing import Any

from tensorplate_pytorch_backend.backends.base import (
    Backend,
    BackendError,
    NamedTensor,
    RuntimeCapability,
)
from tensorplate_pytorch_backend.protocol import (
    ERR_NOT_READY,
    ERR_SHAPE_MISMATCH,
)


class FixtureBackend(Backend):
    """No-dependency echo backend used by sidecar contract tests."""

    def __init__(self) -> None:
        self._loaded = False
        self._primed = False
        self.fail_load: tuple[str, str] | None = None
        self.fail_prime: tuple[str, str] | None = None
        self.fail_infer: tuple[str, str] | None = None
        self.cancelled_request_ids: list[str] = []

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
        # Accept any model_spec; the fixture has no artifact to load.
        _ = model_spec
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


__all__ = ["FixtureBackend"]
