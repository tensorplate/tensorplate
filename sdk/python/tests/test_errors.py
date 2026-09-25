"""The SDK's ErrorCode enum against the wire schemas it decodes."""

from __future__ import annotations

import json
from pathlib import Path

from tensorplate.errors import ErrorCode

_SCHEMAS = Path(__file__).resolve().parents[3] / "protocol" / "schemas"


def _enum(schema: str, *path: str) -> list[str]:
    node = json.loads((_SCHEMAS / schema).read_text(encoding="utf-8"))
    for key in path:
        node = node[key]
    values = node["enum"]
    assert isinstance(values, list)
    return [str(v) for v in values]


def test_error_code_members_match_the_error_schema_in_order() -> None:
    # Order-sensitive: codes are appended, never reordered.
    assert [c.value for c in ErrorCode] == _enum("error.json", "properties", "code")


def test_error_code_members_match_the_health_schema() -> None:
    assert [c.value for c in ErrorCode] == _enum(
        "serving_health.json", "properties", "last_error_code"
    )
