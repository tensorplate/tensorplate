"""Helpers for sidecar-private JSON artifact configuration."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any


class ArtifactConfigError(ValueError):
    """A JSON artifact cannot be used as sidecar configuration."""


def read_artifact_config(model_spec: dict[str, Any]) -> dict[str, Any]:
    """Read a JSON model artifact when it carries sidecar configuration."""

    artifact_path = model_spec.get("artifact_path")
    if not isinstance(artifact_path, str) or not artifact_path:
        return {}
    path = Path(artifact_path)
    if path.suffix.lower() != ".json":
        return {}
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise ArtifactConfigError(f"sidecar config not found: {path}") from None
    except json.JSONDecodeError as exc:
        raise ArtifactConfigError(f"sidecar config JSON invalid: {exc}") from exc
    if not isinstance(raw, dict):
        raise ArtifactConfigError("sidecar config must be a JSON object")
    return raw


__all__ = ["ArtifactConfigError", "read_artifact_config"]
