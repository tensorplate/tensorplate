#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Standard-library reference for canonical JSON version 1.

Writes protocol/fixtures/canonical_json.json and the three
deployment_descriptor_*.json fixtures without the Rust implementation, so
their expected texts and digests do not come from the code they test.
With --check it compares the committed files instead of writing them.
"""

from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path
from typing import Any

REPO = Path(__file__).resolve().parents[2]
FIXTURES = REPO / "protocol" / "fixtures"
MAX_INTEGER = (1 << 53) - 1
MAX_DEPTH = 64
BUDGET_LINES = [
    "model_weights_bytes",
    "runtime_overhead_bytes",
    "session_scratch_bytes",
    "cache_bytes",
    "step_scratch_bytes",
    "output_queue_bytes",
    "io_buffer_bytes",
    "per_session_state_bytes",
    "sidecar_process_bytes",
    "os_reserve_bytes",
    "backend_reserve_bytes",
]


class Refused(Exception):
    def __init__(self, reason: str) -> None:
        super().__init__(reason)
        self.reason = reason


def _pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    if len({key for key, _ in pairs}) != len(pairs):
        raise Refused("duplicate_key")
    return dict(pairs)


def _integer(lexeme: str) -> int:
    value = int(lexeme)
    if lexeme == "-0" or abs(value) > MAX_INTEGER:
        raise Refused("number")
    return value


def _fraction(_lexeme: str) -> float:
    raise Refused("number")


def _constant(_name: str) -> float:
    raise Refused("malformed")  # NaN and Infinity are not JSON.


def _check_value(value: Any, depth: int = 0) -> None:
    if value is None:
        raise Refused("null")
    if isinstance(value, (dict, list)):
        if depth >= MAX_DEPTH:
            raise Refused("depth")
        children = value.values() if isinstance(value, dict) else value
        for child in children:
            _check_value(child, depth + 1)


def canonical_bytes(value: Any) -> bytes:
    _check_value(value)
    text = json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    try:
        return text.encode("utf-8")
    except UnicodeEncodeError as e:  # an unpaired surrogate
        raise Refused("malformed") from e


def canonicalize(text: str) -> bytes:
    try:
        value = json.loads(
            text,
            object_pairs_hook=_pairs,
            parse_int=_integer,
            parse_float=_fraction,
            parse_constant=_constant,
        )
    except Refused:
        raise
    except ValueError as e:
        raise Refused("malformed") from e
    return canonical_bytes(value)


def sha256(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


ACCEPT = [
    ("empty_object", "{}"),
    ("empty_array", "[]"),
    ("top_level_string", '"x"'),
    ("top_level_zero", "0"),
    ("whitespace_between_tokens_is_removed", ' { "b" : 1 ,\n\t"a" : [ true , false ] } \r\n'),
    ("keys_sort_recursively", '{"b":{"z":1,"y":2},"a":{"d":[{"k":1,"j":2}],"c":0}}'),
    ("keys_sort_by_code_point", '{"a":1,"_":2,"B":3,"0":4}'),
    ("a_key_sorts_before_its_extensions", '{"ab":1,"a":2,"a\\u0000":3}'),
    ("supplementary_keys_sort_after_the_private_use_area", '{"\\ud800\\udc00":1,"\\ue000":2}'),
    ("array_order_is_preserved", "[3,1,2,[2,1]]"),
    ("integers_at_the_interoperable_bounds", "[0,1,-1,9007199254740991,-9007199254740991]"),
    ("short_escapes", '"\\"\\\\\\/\\b\\f\\n\\r\\t"'),
    ("other_controls_use_lowercase_hex", '"\\u0000\\u001F\\u000b\\u007f"'),
    ("non_ascii_is_written_as_utf8", '"\\u00e9\\u20AC\\ud834\\udd1e\\u2028"'),
    ("literal_utf8_input_is_kept", '"café — \U0001d11e"'),
    ("booleans", '{"t":true,"f":false}'),
    ("arrays_nested_64_deep", "[" * 64 + "]" * 64),
]

REJECT = [
    ("null_member", '{"a":null}', "null"),
    ("null_array_element", "[1,null]", "null"),
    ("null_document", "null", "null"),
    ("fraction", '{"a":1.5}', "number"),
    ("integral_fraction", '{"a":1.0}', "number"),
    ("exponent", '{"a":1e2}', "number"),
    ("exponent_beyond_a_double", '{"a":1e400}', "number"),
    ("negative_zero", "-0", "number"),
    ("above_the_interoperable_range", "9007199254740992", "number"),
    ("below_the_interoperable_range", "-9007199254740992", "number"),
    ("beyond_64_bits", "18446744073709551616", "number"),
    ("integer_beyond_a_double", "1" + "0" * 400, "number"),
    ("duplicate_key", '{"a":1,"a":1}', "duplicate_key"),
    ("duplicate_nested_key", '{"a":{"b":1,"b":2}}', "duplicate_key"),
    ("duplicate_key_after_unescaping", '{"a":1,"\\u0061":2}', "duplicate_key"),
    ("arrays_nested_65_deep", "[" * 65 + "]" * 65, "depth"),
    ("objects_nested_65_deep", '{"a":' * 65 + "1" + "}" * 65, "depth"),
    ("arrays_nested_128_deep", "[" * 128 + "]" * 128, "depth"),
    ("unpaired_high_surrogate", '"\\ud800"', "malformed"),
    ("unpaired_low_surrogate", '"\\udc00"', "malformed"),
    ("raw_control_character_in_string", '"a\tb"', "malformed"),
    ("trailing_text", "{} x", "malformed"),
    ("two_documents", "{}{}", "malformed"),
    ("leading_zero", "01", "malformed"),
    ("single_quotes", "{'a':1}", "malformed"),
    ("not_a_number_literal", "NaN", "malformed"),
    ("infinity_literal", "Infinity", "malformed"),
    ("byte_order_mark", "﻿{}", "malformed"),
    ("empty_text", "", "malformed"),
]

REASONS = {
    "malformed": "not JSON text: a grammar error, trailing text, a byte order mark, or a string escape that is not a Unicode scalar value",
    "duplicate_key": "an object names the same key twice, compared after unescaping",
    "null": "null anywhere; an absent value is omitted instead",
    "number": "a number that is not an integer written without fraction or exponent in [-(2^53-1), 2^53-1], including -0",
    "depth": f"arrays and objects nested more than {MAX_DEPTH} deep",
}


def vectors() -> dict[str, Any]:
    accept = []
    for name, text in ACCEPT:
        data = canonicalize(text)
        accept.append({"name": name, "input": text, "canonical": data.decode("utf-8"), "sha256": sha256(data)})
    reject = []
    for name, text, reason in REJECT:
        try:
            canonicalize(text)
        except Refused as e:
            if e.reason != reason:
                raise SystemExit(f"{name}: refused with {e.reason}, listed as {reason}") from e
        else:
            raise SystemExit(f"{name}: accepted, listed as refused")
        reject.append({"name": name, "input": text, "reason": reason})
    return {
        "description": "Cross-language vectors for TensorPlate canonical JSON version 1, the form the deployment descriptor's configuration_digest and descriptor_digest are computed over (docs/bundles/integrity.md). Each input is JSON text. An accepted input's canonical value is the exact canonical text once this file is parsed, and sha256 is the digest of its UTF-8 bytes; this file is ASCII, so non-ASCII characters are escaped here and written literally in the canonical text. A rejected input has no canonical form, for the reason named. Every implementation of the form replays every vector. canonical_json_reference.py, a standard-library implementation, writes this file.",
        "canonical_json_version": 1,
        "reasons": REASONS,
        "accept": accept,
        "reject": reject,
    }


def read_json(relative: str) -> Any:
    return json.loads((REPO / relative).read_text(encoding="utf-8"))


def manifest_digest(manifest: dict[str, Any]) -> str:
    # The manifest digest's own form: sorted keys and no whitespace, with
    # manifest_digest stripped. Manifests may carry null, so it is not
    # canonical JSON.
    body = {k: v for k, v in manifest.items() if k != "manifest_digest"}
    return sha256(json.dumps(body, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8"))


def descriptor(bundle: str, deployment_id: str, generation: int, mode: str, quota: dict[str, Any]) -> dict[str, Any]:
    manifest = read_json(f"test/models/bundles/v0_2/{bundle}/manifest.json")
    installed = read_json("protocol/rust/tests/fixtures/backend_descriptor_runner_profiles.json")["runner_profiles"]
    runner = next(p for p in installed if p["id"] == manifest["runner_profile"])
    configuration: dict[str, Any] = {
        "bundle": {
            "name": manifest["name"],
            "version": manifest["version"],
            "format_version": manifest["format_version"],
            "bundle_digest": manifest_digest(manifest),
        },
        "model_class": manifest["model_class"],
        "backend_hint": manifest["backend_hint"],
        "precision_hint": manifest["precision_hint"],
        "runtime_version": "0.3.1",
        "artifacts": [
            {k: a[k] for k in ("role", "kind", "path", "digest", "byte_size") if k in a} for a in manifest["artifacts"]
        ],
        "runner_profile": {"id": runner["id"], "packages": runner["packages"]},
        "compute_type": manifest["compute_type"],
    }
    if "warmup" in manifest:
        configuration["warmup"] = manifest["warmup"]
    configuration["pipeline_stages"] = [
        dict(stage, observable=stage.get("observable", True)) for stage in manifest["pipeline_stages"]
    ]
    configuration["memory_budget_by_domain"] = {
        domain: {line: lines.get(line, 0) for line in BUDGET_LINES}
        for domain, lines in manifest["memory_budget_by_domain"].items()
    }
    configuration["max_concurrent_sessions"] = manifest["max_concurrent_sessions"]
    if manifest.get("degraded_profile") is not None:
        configuration["degraded_profile"] = manifest["degraded_profile"]
    configuration["speech"] = manifest["model_blocks"]["speech"]
    configuration["quota"] = quota
    document: dict[str, Any] = {
        "schema_version": "0.1",
        "canonical_json_version": 1,
        "configuration": configuration,
        "configuration_digest": sha256(canonical_bytes(configuration)),
        "deployment_id": deployment_id,
        "generation": generation,
        "admission_mode": mode,
        "staged_path": f"/var/lib/tensorplate/bundles/staging/{deployment_id}/{generation}",
        "runner_environment": {
            k: runner[k] for k in ("interpreter", "environment_root", "library_search_paths") if k in runner
        },
    }
    document["descriptor_digest"] = sha256(canonical_bytes(document))
    return document


def outputs() -> dict[str, Any]:
    stt_quota = {"session_count": 1, "domain_bytes": {"guest_ram": 33554432, "device_vram": 67108864}}
    tts_quota = {"session_count": 1, "domain_bytes": {"guest_ram": 16777216, "device_vram": 33554432}}
    return {
        "canonical_json.json": vectors(),
        "deployment_descriptor_stt.json": descriptor("speech_stt_streaming", "speech-stt", 3, "qualification", stt_quota),
        "deployment_descriptor_stt_restart.json": descriptor(
            "speech_stt_streaming", "speech-stt", 4, "qualification", stt_quota
        ),
        "deployment_descriptor_tts.json": descriptor("speech_tts_streaming", "speech-tts", 5, "production", tts_quota),
    }


def main() -> int:
    check = sys.argv[1:] == ["--check"]
    stale = []
    for name, document in outputs().items():
        text = json.dumps(document, indent=2, ensure_ascii=True) + "\n"
        path = FIXTURES / name
        if check:
            if path.read_text(encoding="ascii") != text:
                stale.append(name)
        else:
            path.write_text(text, encoding="ascii")
    for name in stale:
        print(f"{name} differs from what the reference writes", file=sys.stderr)
    return 1 if stale else 0


if __name__ == "__main__":
    sys.exit(main())
