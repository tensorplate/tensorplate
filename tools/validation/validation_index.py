# SPDX-License-Identifier: Apache-2.0
"""The validation run index: derive a line from what a run recorded, and compare two.

A line's shape is `config/schemas/validation_run_index_line.json`. A line is
derived from a lifecycle report, a candidate qualification record or doctor's
JSON output. A field the source's own schema requires and that is absent is
an error; a value that was not measured yields no metric, so the comparison
reports it as missing instead of unchanged.
"""

from __future__ import annotations

import json
import math
from datetime import datetime
from pathlib import Path
from typing import Any

import candidate_record

REPO_ROOT = Path(__file__).resolve().parents[2]
SCHEMA_PATH = REPO_ROOT / "config/schemas/validation_run_index_line.json"
DEFAULT_TOLERANCE_PCT = 10.0
# Metrics where lower is better; any other metric is a count, and a count that moves is `changed`.
COST_SUFFIXES = ("_ms", "_s", "_mib", ".rtf_median")
EXIT_UNCHANGED, EXIT_MOVED, EXIT_NOT_COMPARABLE = 0, 1, 2

_RUNNER_FINDINGS = ("runner_profiles", "runner_profile_dependencies", "runner_launch_environment")
# The statuses a pass accepts per finding, by whether a speech runtime family
# is installed: `failing == 0` alone lets a warning or a skipped check through.
DOCTOR_REQUIREMENTS: dict[str, dict[str, tuple[str, ...]]] = {
    "installed": {
        "platform_row": ("ok",),
        "python_pytorch_runtime": ("ok", "missing"),
        **{finding: ("ok",) for finding in _RUNNER_FINDINGS},
    },
    "none": {
        "platform_row": ("ok",),
        "python_pytorch_runtime": ("ok",),
        **{finding: ("skipped",) for finding in _RUNNER_FINDINGS},
    },
}
SPEECH_FAMILY_MODES = {"in-assets": "installed", "host-built": "installed", "none": "none"}


class IndexLineError(Exception):
    """A line cannot be derived, does not conform, or cannot be compared."""


def load_schema(path: Path = SCHEMA_PATH) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def validate_line(line: Any, schema: dict[str, Any] | None = None) -> None:
    try:
        candidate_record.validate_record(line, load_schema() if schema is None else schema)
    except candidate_record.RecordError as exc:
        raise IndexLineError(f"index line: {exc}") from exc
    # JSON Schema's `number` admits NaN and Infinity, which compare equal to nothing.
    for key, value in line["metrics"].items():
        if not math.isfinite(value):
            raise IndexLineError(f"index line: /metrics/{key}: {value!r} is not a finite number")


def new_line(
    envelope: dict[str, Any],
    subject: str,
    result: str,
    metrics: dict[str, float],
    checks: dict[str, str],
    family_build: str | None = None,
) -> dict[str, Any]:
    """Join a subject's derived values with the session facts the operator supplies."""
    parts = (envelope["date"], envelope["row"], subject, envelope.get("run_suffix"))
    line: dict[str, Any] = {
        "run_id": "-".join(part for part in parts if part),
        "date": envelope["date"],
        "session": envelope["session"],
        "row": envelope["row"],
        "machine": envelope["machine"],
        "build": envelope["build"],
    }
    if family_build:
        line["family_build"] = family_build
    line.update(
        subject=subject,
        kind=envelope["kind"],
        result=result,
        metrics=metrics,
        checks=checks,
        settings=dict(envelope["settings"]),
        records=envelope["records"],
        raw=envelope["raw"],
        notes=envelope["notes"],
    )
    validate_line(line)
    return line


def _put(target: dict[str, Any], key: str, value: Any) -> None:
    """Set a derived value once: a second writer means the source names one thing twice."""
    if key in target:
        raise IndexLineError(f"the source yields {key!r} twice")
    target[key] = value


def _elapsed_s(started: str, finished: str) -> int:
    def parse(text: str) -> datetime:
        return datetime.fromisoformat(text.replace("Z", "+00:00"))

    return int((parse(finished) - parse(started)).total_seconds())


def lifecycle_line(report: dict[str, Any], envelope: dict[str, Any]) -> dict[str, Any]:
    if report["row_id"] != envelope["row"]:
        raise IndexLineError(
            f"the lifecycle report is for row {report['row_id']!r}, not {envelope['row']!r}"
        )
    stages = report["stages"]
    metrics: dict[str, float] = {
        "stages_passed": sum(1 for stage in stages if stage["status"] == "pass"),
        "stages_total": len(stages),
        "harness_wall_s": _elapsed_s(report["started_at"], report["finished_at"]),
    }
    checks: dict[str, str] = {}
    for stage in stages:
        _put(checks, f"stage.{stage['stage']}", stage["status"])
    if "reboot" in report:
        _put(checks, "stage.reboot", report["reboot"]["status"])
    return new_line(envelope, "lifecycle", report["outcome"], metrics, checks)


def _mib(byte_count: int) -> float:
    return round(byte_count / (1 << 20), 1)


def qualification_line(
    record: dict[str, Any],
    envelope: dict[str, Any],
    subject: str,
    family_build: str | None = None,
    cold_deploy: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """`cold_deploy` is the pass driver's `cold-deploy.json` for this candidate, when it ran."""
    try:
        candidate_record.validate_record(record)
    except candidate_record.RecordError as exc:
        raise IndexLineError(f"{subject}: the qualification record is malformed: {exc}") from exc
    if record["provenance"] != "recorded":
        raise IndexLineError(f"{subject}: a {record['provenance']} record measured nothing")
    metrics: dict[str, float] = {}
    # A record indexed under another candidate's subject shows as a changed bundle.
    checks: dict[str, str] = {"bundle": record["subject"]["bundle"]["name"]}
    lifecycle = record["lifecycle"]

    deploy = lifecycle["deploy"]
    checks["deploy"] = deploy["status"]
    if deploy["status"] == "ok":
        metrics["deploy_wall_ms"] = deploy["wall_ms"]
    if cold_deploy is not None:
        _cold_deploy(subject, cold_deploy, deploy, metrics, checks)

    # The runner reports its one load with every answer; two values mean two loads.
    loads = {
        timings["load"]
        for fixture in record["fixtures"]
        for sample in fixture["samples"]
        if "load" in (timings := sample["runner_timings_us"])
    }
    if len(loads) > 1:
        raise IndexLineError(f"{subject}: {len(loads)} runner load timings in one record")
    if loads:
        metrics["runner_load_ms"] = round(loads.pop() / 1000, 1)

    # The recipe's first request after the deploy is its first fixture's first timing sample.
    for sample in record["fixtures"][0]["samples"] if record["fixtures"] else []:
        if (sample["phase"], sample["iteration"], sample["status"]) == ("timing", 0, "ok"):
            _put(metrics, "first_request_ms", round(sample["exchange_wall_us"] / 1000, 1))

    with_failures = 0
    for fixture in record["fixtures"]:
        summary = fixture["summary"]
        with_failures += 1 if summary["failed_count"] else 0
        if summary["rtf"] is not None:
            _put(metrics, f"fixture.{fixture['id']}.rtf_median", summary["rtf"]["median"])
        if summary["exchange_wall_us"] is not None:
            wall_ms = round(summary["exchange_wall_us"]["median"] / 1000, 1)
            _put(metrics, f"fixture.{fixture['id']}.exchange_wall_median_ms", wall_ms)
    if not record["fixtures"]:
        checks["fixtures"] = "none ran"
    elif with_failures:
        checks["fixtures"] = f"{with_failures} of {len(record['fixtures'])} had failed samples"
    else:
        checks["fixtures"] = "all ok"

    for name, window in record["memory"]["windows"].items():
        checks[f"memory.{name}"] = window["status"]
        # An incomplete window's maximum is not the window's maximum.
        if window["status"] != "measured":
            continue
        for domain, body in window["domains"].items():
            prefix = f"memory.{name}.{domain}"
            if body["consumed"]["max_bytes"] is not None:
                metrics[f"{prefix}.consumed_max_mib"] = _mib(body["consumed"]["max_bytes"])
            roles = [process["role"] for process in body["processes"]]
            if len(set(roles)) != len(roles):
                raise IndexLineError(f"{subject}: two processes share a role in {prefix}")
            for process in body["processes"]:
                if process["peak"]["max_bytes"] is not None:
                    key = f"{prefix}.{process['role']}_peak_mib"
                    metrics[key] = _mib(process["peak"]["max_bytes"])

    for negative in record["negatives"]:
        observed = negative["observed"]
        code = f" ({observed['code']})" if observed is not None else ""
        _put(checks, f"negative.{negative['case']}", negative["status"] + code)

    teardown = lifecycle["teardown"]
    checks["teardown"] = f"{teardown['status']} ({teardown['method']})"
    for name in ("status_after_deploy", "status_after_negatives"):
        snapshot = lifecycle[name]
        if snapshot is None:
            checks[name] = "not taken"
            continue
        failed = sorted(check for check, ok in snapshot["checks"].items() if not ok)
        checks[name] = "failed: " + ", ".join(failed) if failed else "ok"

    return new_line(envelope, subject, record["result"]["status"], metrics, checks, family_build)


def _cold_deploy(
    subject: str,
    cold: dict[str, Any],
    deploy: dict[str, Any],
    metrics: dict[str, float],
    checks: dict[str, str],
) -> None:
    """A deploy the driver timed right after dropping the page cache."""
    outcome = cold["deploy"]
    failure = outcome["failure"]
    code = f" ({failure['code']})" if failure is not None else ""
    checks["cold_deploy"] = outcome["status"] + code
    if outcome["status"] != "ok" or cold["page_cache"] != "dropped":
        return
    ours, theirs = outcome["bundle_digest"], deploy["bundle_digest"]
    if ours is not None and theirs is not None and ours != theirs:
        raise IndexLineError(f"{subject}: the cold deploy measured another bundle ({ours})")
    # Without both digests nothing says the two deploys were of one bundle.
    if ours is not None and theirs is not None:
        metrics["deploy_wall_cold_cache_ms"] = outcome["wall_ms"]


def doctor_line(
    payload: dict[str, Any],
    envelope: dict[str, Any],
    interpreter_override: bool,
    required: dict[str, tuple[str, ...]] | None = None,
) -> dict[str, Any]:
    """`payload` is the `payload` object of `tensorplate doctor --output json`.

    `required` maps a finding id to the statuses a pass accepts for it.
    """
    required = required or {}
    findings = payload["findings"]
    checks = {f"finding.{finding['id']}": finding["status"] for finding in findings}
    if len(checks) != len(findings):
        raise IndexLineError("doctor reported one finding id twice")
    failed = sum(1 for finding in findings if finding["status"] == "fail")
    if failed != payload["failing"]:
        raise IndexLineError(
            f"doctor's total says {payload['failing']} failing and its findings say {failed}"
        )
    for finding_id in required:
        checks.setdefault(f"finding.{finding_id}", "absent")
    checks["interpreter_override"] = "present" if interpreter_override else "absent"
    passed = (
        payload["failing"] == 0
        and not interpreter_override
        and all(
            checks[f"finding.{finding_id}"] in accepted for finding_id, accepted in required.items()
        )
    )
    metrics = {"findings_total": len(findings), "failing": payload["failing"]}
    return new_line(envelope, "doctor", "pass" if passed else "fail", metrics, checks)


def read_index(path: Path) -> list[dict[str, Any]]:
    """Every line of a JSON Lines index, each one valid and no run id twice."""
    schema = load_schema()
    lines: list[dict[str, Any]] = []
    seen: set[str] = set()
    for number, text in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not text.strip():
            continue
        try:
            line = json.loads(text)
            validate_line(line, schema)
        except (json.JSONDecodeError, IndexLineError) as exc:
            raise IndexLineError(f"{path}:{number}: {exc}") from exc
        if line["run_id"] in seen:
            raise IndexLineError(f"{path}:{number}: run id {line['run_id']!r} appears twice")
        seen.add(line["run_id"])
        lines.append(line)
    return lines


def _is_cost(key: str) -> bool:
    return key.endswith(COST_SUFFIXES)


def compare_lines(
    previous: dict[str, Any],
    current: dict[str, Any],
    tolerance_pct: float = DEFAULT_TOLERANCE_PCT,
) -> list[dict[str, Any]]:
    """One finding per field: unchanged, improved, regressed, changed, missing, new or note."""
    if not (math.isfinite(tolerance_pct) and tolerance_pct >= 0):
        raise IndexLineError(f"a tolerance of {tolerance_pct!r} percent compares nothing")
    for field in ("row", "subject"):
        if previous[field] != current[field]:
            raise IndexLineError(
                f"{field} differs: {previous[field]!r} and {current[field]!r} are not comparable"
            )
    findings: list[dict[str, Any]] = []

    def add(field: str, status: str, before: Any, after: Any) -> None:
        findings.append({"field": field, "status": status, "previous": before, "current": after})

    same = previous["result"] == current["result"]
    add("result", "unchanged" if same else "changed", previous["result"], current["result"])

    for key in sorted(set(previous["metrics"]) | set(current["metrics"])):
        before, after = previous["metrics"].get(key), current["metrics"].get(key)
        if after is None:
            status = "missing"
        elif before is None:
            status = "new"
        elif not _is_cost(key):
            status = "unchanged" if after == before else "changed"
        else:
            limit = abs(before) * tolerance_pct / 100
            if after - before > limit:
                status = "regressed"
            elif before - after > limit:
                status = "improved"
            else:
                status = "unchanged"
        add(f"metrics.{key}", status, before, after)

    for key in sorted(set(previous["checks"]) | set(current["checks"])):
        before, after = previous["checks"].get(key), current["checks"].get(key)
        if after is None:
            status = "missing"
        elif before is None:
            status = "new"
        else:
            status = "unchanged" if after == before else "changed"
        add(f"checks.{key}", status, before, after)

    # Runs under different settings are still compared; the reader is told they differ.
    for field in ("build", "family_build"):
        if previous.get(field) != current.get(field):
            add(field, "note", previous.get(field), current.get(field))
    for key in sorted(set(previous["settings"]) | set(current["settings"])):
        before, after = previous["settings"].get(key), current["settings"].get(key)
        if before != after:
            add(f"settings.{key}", "note", before, after)
    return findings


def verdict(findings: list[dict[str, Any]]) -> int:
    statuses = {finding["status"] for finding in findings}
    if "missing" in statuses:
        return EXIT_NOT_COMPARABLE
    if statuses & {"regressed", "improved", "changed"}:
        return EXIT_MOVED
    return EXIT_UNCHANGED


def select_previous(
    index: list[dict[str, Any]], line: dict[str, Any], current_ids: set[str]
) -> dict[str, Any] | None:
    """The newest line for the same row and subject that is not one of the pass's own."""
    matches = [
        other
        for other in index
        if (other["row"], other["subject"]) == (line["row"], line["subject"])
        and other["run_id"] not in current_ids
    ]
    return matches[-1] if matches else None


def missing_subjects(
    index: list[dict[str, Any]], current: list[dict[str, Any]]
) -> list[tuple[str, str]]:
    """Subjects the row's latest indexed session ran that the current lines do not have."""
    current_ids = {line["run_id"] for line in current}
    missing: list[tuple[str, str]] = []
    for row in sorted({line["row"] for line in current}):
        earlier = [
            line for line in index if line["row"] == row and line["run_id"] not in current_ids
        ]
        if not earlier:
            continue
        session = (earlier[-1]["date"], earlier[-1]["session"])
        ran = {line["subject"] for line in earlier if (line["date"], line["session"]) == session}
        have = {line["subject"] for line in current if line["row"] == row}
        missing.extend((row, subject) for subject in sorted(ran - have))
    return missing


def dump_line(line: dict[str, Any]) -> str:
    return json.dumps(line, ensure_ascii=False, allow_nan=False) + "\n"
