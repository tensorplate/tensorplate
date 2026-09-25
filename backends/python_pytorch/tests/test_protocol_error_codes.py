"""The sidecar's ERR_* constants against protocol/schemas/error.json."""

from __future__ import annotations

import json
from pathlib import Path

from tensorplate_pytorch_backend import protocol

_ERROR_SCHEMA = Path(__file__).resolve().parents[3] / "protocol" / "schemas" / "error.json"


def _err_constants() -> list[tuple[str, str]]:
    # Module attributes keep definition order, so this is source order.
    return [(name, value) for name, value in vars(protocol).items() if name.startswith("ERR_")]


def test_err_constants_match_the_error_schema_in_order() -> None:
    schema = json.loads(_ERROR_SCHEMA.read_text(encoding="utf-8"))
    codes = schema["properties"]["code"]["enum"]
    assert [value for _, value in _err_constants()] == codes


def test_err_constants_are_exported() -> None:
    names = [name for name, _ in _err_constants()]
    assert names
    assert set(names) <= set(protocol.__all__)
