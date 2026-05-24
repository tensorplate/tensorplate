"""Unit tests for SmolVLA backend pure helpers.

These cover config parsing and environment-variable fallbacks. The lazy
torch/numpy/lerobot/transformers dependencies are never imported, so the
suite runs anywhere CPython is available (matching the rest of the
backends package).
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from tensorplate_pytorch_backend.backends.base import BackendError
from tensorplate_pytorch_backend.backends.smolvla import (
    _optional_int_config,
    _read_validation_config,
    _string_config,
)
from tensorplate_pytorch_backend.protocol import ERR_CONFIG_INVALID

# ---------------------------------------------------------------------------
# _read_validation_config
# ---------------------------------------------------------------------------


def test_read_validation_config_no_artifact_path_returns_empty() -> None:
    assert _read_validation_config({}) == {}


def test_read_validation_config_empty_artifact_path_returns_empty() -> None:
    assert _read_validation_config({"artifact_path": ""}) == {}


def test_read_validation_config_non_string_artifact_path_returns_empty() -> None:
    assert _read_validation_config({"artifact_path": 42}) == {}


def test_read_validation_config_non_json_suffix_returns_empty(tmp_path: Path) -> None:
    artifact = tmp_path / "model.bin"
    artifact.write_bytes(b"\x00\x01")
    assert _read_validation_config({"artifact_path": str(artifact)}) == {}


def test_read_validation_config_missing_file_raises(tmp_path: Path) -> None:
    artifact = tmp_path / "missing.json"
    with pytest.raises(BackendError) as exc_info:
        _read_validation_config({"artifact_path": str(artifact)})
    assert getattr(exc_info.value, "code", None) == ERR_CONFIG_INVALID


def test_read_validation_config_invalid_json_raises(tmp_path: Path) -> None:
    artifact = tmp_path / "broken.json"
    artifact.write_text("{not json", encoding="utf-8")
    with pytest.raises(BackendError) as exc_info:
        _read_validation_config({"artifact_path": str(artifact)})
    assert getattr(exc_info.value, "code", None) == ERR_CONFIG_INVALID


def test_read_validation_config_non_object_raises(tmp_path: Path) -> None:
    artifact = tmp_path / "list.json"
    artifact.write_text(json.dumps([1, 2, 3]), encoding="utf-8")
    with pytest.raises(BackendError) as exc_info:
        _read_validation_config({"artifact_path": str(artifact)})
    assert getattr(exc_info.value, "code", None) == ERR_CONFIG_INVALID


def test_read_validation_config_returns_parsed_object(tmp_path: Path) -> None:
    artifact = tmp_path / "cfg.json"
    artifact.write_text(json.dumps({"model_id": "x", "num_steps": 4}), encoding="utf-8")
    parsed = _read_validation_config({"artifact_path": str(artifact)})
    assert parsed == {"model_id": "x", "num_steps": 4}


# ---------------------------------------------------------------------------
# _string_config
# ---------------------------------------------------------------------------


def test_string_config_returns_value_from_config() -> None:
    assert _string_config({"model_id": "abc"}, "model_id", "default") == "abc"


def test_string_config_falls_back_to_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("TP_SMOLVLA_MODEL_ID", "env-value")
    assert _string_config({}, "model_id", "default-value") == "env-value"


def test_string_config_uses_default_when_no_config_or_env(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("TP_SMOLVLA_MODEL_ID", raising=False)
    assert _string_config({}, "model_id", "default-value") == "default-value"


def test_string_config_rejects_empty_string() -> None:
    with pytest.raises(BackendError) as exc_info:
        _string_config({"model_id": ""}, "model_id", "default")
    assert getattr(exc_info.value, "code", None) == ERR_CONFIG_INVALID


def test_string_config_rejects_non_string() -> None:
    with pytest.raises(BackendError) as exc_info:
        _string_config({"model_id": 123}, "model_id", "default")
    assert getattr(exc_info.value, "code", None) == ERR_CONFIG_INVALID


# ---------------------------------------------------------------------------
# _optional_int_config
# ---------------------------------------------------------------------------


def test_optional_int_config_returns_none_when_unset(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("TP_SMOLVLA_NUM_STEPS", raising=False)
    assert _optional_int_config({}, "num_steps") is None


def test_optional_int_config_returns_none_for_empty_string() -> None:
    assert _optional_int_config({"num_steps": ""}, "num_steps") is None


def test_optional_int_config_parses_string_from_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("TP_SMOLVLA_NUM_STEPS", "7")
    assert _optional_int_config({}, "num_steps") == 7


def test_optional_int_config_parses_integer_from_config() -> None:
    assert _optional_int_config({"num_steps": 12}, "num_steps") == 12


def test_optional_int_config_rejects_non_integer_string() -> None:
    with pytest.raises(BackendError) as exc_info:
        _optional_int_config({"num_steps": "not-a-number"}, "num_steps")
    assert getattr(exc_info.value, "code", None) == ERR_CONFIG_INVALID


def test_optional_int_config_rejects_zero() -> None:
    with pytest.raises(BackendError) as exc_info:
        _optional_int_config({"num_steps": 0}, "num_steps")
    assert getattr(exc_info.value, "code", None) == ERR_CONFIG_INVALID


def test_optional_int_config_rejects_negative() -> None:
    with pytest.raises(BackendError) as exc_info:
        _optional_int_config({"num_steps": -5}, "num_steps")
    assert getattr(exc_info.value, "code", None) == ERR_CONFIG_INVALID
