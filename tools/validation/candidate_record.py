# SPDX-License-Identifier: Apache-2.0
"""The candidate qualification record: build, finalize and validate.

The record's shape is `config/schemas/candidate_qualification_record.json`.
This module carries a small draft-07 validator for the subset that schema
uses, so the qualify tool fails closed on a malformed record without a
third-party dependency; the test suite cross-checks it against the
`jsonschema` library.
"""

from __future__ import annotations

import hashlib
import json
import re
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
SCHEMA_PATH = REPO_ROOT / "config/schemas/candidate_qualification_record.json"
ERROR_SCHEMA_PATH = REPO_ROOT / "protocol/schemas/error.json"

RECORD_KIND = "candidate_qualification"
CONVENTION = "candidate_only_text_utf8_result_json"
TOOL_NAME = "candidate-qualify"
SAMPLER_ENTRY = "tools/validation/memory-sample.sh"
WINDOW_NAMES = ("predecessor_idle", "candidate_warm_idle", "candidate_load", "after_teardown")
WINDOW_PHASES = {
    "predecessor_idle": "warm-idle",
    "candidate_warm_idle": "warm-idle",
    "candidate_load": "load",
    "after_teardown": "warm-idle",
}


class RecordError(Exception):
    """The record does not conform to its schema."""


def sha256_digest(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return "sha256:" + digest.hexdigest()


def load_schema(path: Path = SCHEMA_PATH) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def load_error_codes(path: Path = ERROR_SCHEMA_PATH) -> frozenset[str]:
    schema = json.loads(path.read_text(encoding="utf-8"))
    return frozenset(schema["properties"]["code"]["enum"])


def nearest_rank_distribution(values: list[float]) -> dict[str, Any] | None:
    if not values:
        return None
    ordered = sorted(values)
    count = len(ordered)
    # Nearest rank at p = 0.5: ceil(0.5 * n), 1-indexed.
    rank = (count + 1) // 2
    return {"count": count, "min": ordered[0], "median": ordered[rank - 1], "max": ordered[-1]}


def empty_window(name: str, reason: str) -> dict[str, Any]:
    return {
        "status": "not_run",
        "reason": reason,
        "phase": WINDOW_PHASES[name],
        "observations": [],
        "report": None,
        "domains": {},
    }


def new_record(
    *,
    provenance: str,
    recorded_at_utc: str,
    source_commit: str | None,
    transport_preference: str,
    request_timeout_ms: int,
    subject: dict[str, Any],
    deployment_id: str,
    pending_reason: str,
) -> dict[str, Any]:
    """A record with every step `not_run`; the recipe fills steps as it runs."""
    return {
        "schema_version": "0.1",
        "record_kind": RECORD_KIND,
        "convention": CONVENTION,
        "provenance": provenance,
        "qualification": {"presented_as": "candidate", "production_evidence": False},
        "tool": {
            "name": TOOL_NAME,
            "source_commit": source_commit,
            "transport_preference": transport_preference,
        },
        "recorded_at_utc": recorded_at_utc,
        "request_timeout_ms": request_timeout_ms,
        "subject": subject,
        "device_facts": {
            "source": "not_observed",
            "device": None,
            "compute_type_loaded": None,
            "engine_versions": {},
            "cli_version": None,
            "protocol_version": None,
            "bundle_format_version": None,
        },
        "lifecycle": {
            "predecessor": None,
            "deploy": {
                "deployment_id": deployment_id,
                "transaction_id": None,
                "phase": None,
                "bundle_digest": None,
                "wall_ms": 0,
                "status": "not_run",
                "failure": None,
            },
            "status_after_deploy": None,
            "status_after_negatives": None,
            "teardown": {
                "method": "none",
                "status": "not_run",
                "restored_deployment_id": None,
                "wall_ms": 0,
                "reason": pending_reason,
            },
        },
        "memory": {
            "sampler": None,
            "processes": [],
            "windows": {name: empty_window(name, pending_reason) for name in WINDOW_NAMES},
        },
        "fixtures": [],
        "negatives": [],
        "result": {"status": "incomplete", "reasons": [pending_reason]},
    }


def finalize(record: dict[str, Any], stop_reason: str | None = None) -> dict[str, Any]:
    """Derive `result` from the recorded steps; nothing else is recomputed."""
    reasons: list[str] = [] if stop_reason is None else [f"stopped: {stop_reason}"]
    failed = False
    lifecycle = record["lifecycle"]
    for step in ("predecessor", "deploy", "teardown"):
        entry = lifecycle[step]
        if entry is None:
            if step == "predecessor":
                reasons.append("no predecessor deployment: teardown by rollback not exercised")
            continue
        if entry["status"] == "failed":
            failed = True
            reasons.append(f"lifecycle step {step} failed")
        elif entry["status"] == "not_run":
            reasons.append(f"lifecycle step {step} did not run")
    for snapshot_name in ("status_after_deploy", "status_after_negatives"):
        snapshot = lifecycle[snapshot_name]
        if snapshot is None:
            reasons.append(f"{snapshot_name} was not taken")
        elif not all(snapshot["checks"].values()):
            failed = True
            bad = sorted(name for name, ok in snapshot["checks"].items() if not ok)
            reasons.append(f"{snapshot_name} checks failed: {', '.join(bad)}")
    if not record["fixtures"]:
        reasons.append("no fixture ran")
    for fixture in record["fixtures"]:
        if fixture["summary"]["failed_count"]:
            failed = True
            reasons.append(f"fixture {fixture['id']} had failed samples")
        if not fixture["summary"]["ok_count"]:
            reasons.append(f"fixture {fixture['id']} has no successful sample")
        for sample in fixture["samples"]:
            if sample["status"] == "ok" and not sample["within_request_timeout"]:
                failed = True
                reasons.append(
                    f"fixture {fixture['id']} sample {sample['iteration']} exceeded the request "
                    "timeout"
                )
    for negative in record["negatives"]:
        if negative["status"] == "mismatched":
            failed = True
            reasons.append(f"negative case {negative['case']} observed an unexpected outcome")
        elif negative["status"] == "not_run":
            reasons.append(f"negative case {negative['case']} did not run")
    for name, window in record["memory"]["windows"].items():
        if window["status"] == "incomplete":
            reasons.append(f"memory window {name} has incomplete coverage")
        elif window["status"] == "not_run":
            reasons.append(f"memory window {name} did not run")
    if failed:
        status = "fail"
    elif reasons:
        status = "incomplete"
    else:
        status = "pass"
    record["result"] = {"status": status, "reasons": reasons}
    return record


# --- schema validation (draft-07 subset) -----------------------------------


def validate_record(record: Any, schema: dict[str, Any] | None = None) -> None:
    """Raise RecordError at the first schema violation, naming its JSON pointer."""
    schema = load_schema() if schema is None else schema
    _validate(record, schema, schema, "")


def validate_error_codes(record: dict[str, Any], codes: frozenset[str]) -> None:
    """Every observed error code and every expected code is one the wire defines."""
    seen: list[tuple[str, str]] = []
    failure = record["lifecycle"]["deploy"]["failure"]
    if failure is not None:
        seen.append(("/lifecycle/deploy/failure/code", failure["code"]))
    for name in ("status_after_deploy", "status_after_negatives"):
        snapshot = record["lifecycle"][name]
        if snapshot is not None and snapshot["last_error"] is not None:
            seen.append((f"/lifecycle/{name}/last_error/code", snapshot["last_error"]["code"]))
    for index, negative in enumerate(record["negatives"]):
        for code in negative["expected_codes"]:
            seen.append((f"/negatives/{index}/expected_codes", code))
        if negative["observed"] is not None:
            seen.append((f"/negatives/{index}/observed/code", negative["observed"]["code"]))
    for findex, fixture in enumerate(record["fixtures"]):
        for sindex, sample in enumerate(fixture["samples"]):
            if sample["error"] is not None:
                seen.append(
                    (f"/fixtures/{findex}/samples/{sindex}/error/code", sample["error"]["code"])
                )
    for pointer, code in seen:
        if code not in codes:
            raise RecordError(f"{pointer}: {code!r} is not an error code the protocol defines")


_TYPE_CHECKS = {
    "object": lambda v: isinstance(v, dict),
    "array": lambda v: isinstance(v, list),
    "string": lambda v: isinstance(v, str),
    "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
    "number": lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
    "boolean": lambda v: isinstance(v, bool),
    "null": lambda v: v is None,
}


def _resolve_ref(ref: str, root: dict[str, Any]) -> dict[str, Any]:
    if not ref.startswith("#/"):
        raise RecordError(f"unsupported $ref {ref!r}")
    node: Any = root
    for part in ref[2:].split("/"):
        node = node[part]
    return node


def _validate(value: Any, schema: dict[str, Any], root: dict[str, Any], pointer: str) -> None:
    if "$ref" in schema:
        _validate(value, _resolve_ref(schema["$ref"], root), root, pointer)
        return
    if "oneOf" in schema:
        matches = 0
        for branch in schema["oneOf"]:
            try:
                _validate(value, branch, root, pointer)
            except RecordError:
                continue
            matches += 1
        if matches != 1:
            raise RecordError(
                f"{pointer or '/'}: matched {matches} oneOf branches, not exactly one"
            )
    if "type" in schema:
        allowed = schema["type"] if isinstance(schema["type"], list) else [schema["type"]]
        if not any(_TYPE_CHECKS[t](value) for t in allowed):
            raise RecordError(
                f"{pointer or '/'}: expected type {allowed}, found {type(value).__name__}"
            )
    if "const" in schema and value != schema["const"]:
        raise RecordError(f"{pointer or '/'}: expected {schema['const']!r}, found {value!r}")
    if "enum" in schema and value not in schema["enum"]:
        raise RecordError(f"{pointer or '/'}: {value!r} is not one of {schema['enum']!r}")
    if isinstance(value, str):
        if "minLength" in schema and len(value) < schema["minLength"]:
            raise RecordError(f"{pointer or '/'}: string shorter than {schema['minLength']}")
        if "pattern" in schema and re.search(schema["pattern"], value) is None:
            raise RecordError(f"{pointer or '/'}: {value!r} does not match {schema['pattern']!r}")
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        if "minimum" in schema and value < schema["minimum"]:
            raise RecordError(f"{pointer or '/'}: {value!r} is below {schema['minimum']}")
    if isinstance(value, list):
        if "minItems" in schema and len(value) < schema["minItems"]:
            raise RecordError(f"{pointer or '/'}: fewer than {schema['minItems']} items")
        if "items" in schema:
            for index, item in enumerate(value):
                _validate(item, schema["items"], root, f"{pointer}/{index}")
    if isinstance(value, dict):
        for key in schema.get("required", []):
            if key not in value:
                raise RecordError(f"{pointer or '/'}: missing required field {key!r}")
        properties = schema.get("properties", {})
        for key, child in value.items():
            if key in properties:
                _validate(child, properties[key], root, f"{pointer}/{key}")
            elif "additionalProperties" in schema:
                extra = schema["additionalProperties"]
                if extra is False:
                    raise RecordError(f"{pointer or '/'}: unexpected field {key!r}")
                if isinstance(extra, dict):
                    _validate(child, extra, root, f"{pointer}/{key}")


def dump(record: dict[str, Any]) -> str:
    return json.dumps(record, indent=2, sort_keys=False, ensure_ascii=False) + "\n"
