"""MPS-backed fixture used by the packaged macOS deployment gate."""

from __future__ import annotations

from typing import Any

from tensorplate_pytorch_backend.accelerator import require_mps_runtime
from tensorplate_pytorch_backend.backends.base import BackendError, RuntimeCapability
from tensorplate_pytorch_backend.backends.fixture import FixtureBackend
from tensorplate_pytorch_backend.configuration import ArtifactConfigError, read_artifact_config
from tensorplate_pytorch_backend.protocol import ERR_CONFIG_INVALID, ERR_LOAD_FAILED


class MpsFixtureBackend(FixtureBackend):
    """Echo fixture whose load succeeds only after an MPS tensor operation."""

    def __init__(self) -> None:
        super().__init__()
        self._runtime_capability: RuntimeCapability | None = None

    @property
    def name(self) -> str:
        return "mps_fixture"

    @property
    def runtime_capability(self) -> RuntimeCapability | None:
        return self._runtime_capability

    def load(self, model_spec: dict[str, Any]) -> None:
        self._runtime_capability = None
        try:
            config = read_artifact_config(model_spec)
        except ArtifactConfigError as exc:
            raise BackendError(ERR_CONFIG_INVALID, str(exc)) from exc
        if config.get("device") != "mps":
            raise BackendError(
                ERR_CONFIG_INVALID,
                "MPS fixture config requires device='mps'",
            )
        try:
            import torch
        except Exception as exc:
            raise BackendError(ERR_LOAD_FAILED, "PyTorch is required for the MPS fixture") from exc

        self._runtime_capability = require_mps_runtime(torch)
        try:
            probe = torch.ones((1,), device="mps")
            _ = probe + probe
            torch.mps.synchronize()
        except Exception as exc:
            raise BackendError(
                ERR_LOAD_FAILED,
                f"MPS fixture tensor operation failed: {exc}",
                runtime_capability=self._runtime_capability,
            ) from exc
        super().load(model_spec)

    def unload(self) -> None:
        super().unload()
        self._runtime_capability = None


__all__ = ["MpsFixtureBackend"]
