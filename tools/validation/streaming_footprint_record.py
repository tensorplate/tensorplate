#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The streaming footprint record: declared budgets, assembly from raw captures, the check.

The budgets live in `tools/validation/streaming-footprint-thresholds.json` and
are copied into every record as read; the check refuses a record whose copy
differs from that file. Every derived field (per-run values, maximum deltas,
budget statuses, the verdict) is recomputed from the raw measurements and
compared with what the record claims, so a record cannot say more than its
samples support. The record's shape is
`config/schemas/streaming_footprint_record.json`.

Exit status of `check`: 0 pass, 1 fail (a budget exceeded), 2 no verdict (the
record is incomplete or malformed).
"""

from __future__ import annotations

import argparse
import datetime as _dt
import hashlib
import json
import socket
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
THRESHOLDS_PATH = HERE / "streaming-footprint-thresholds.json"
SCHEMA_PATH = REPO_ROOT / "config/schemas/streaming_footprint_record.json"

sys.path.insert(0, str(HERE))
from candidate_record import RecordError, validate_record  # noqa: E402

TOOL_NAME = "streaming-footprint"
RECORD_KIND = "streaming_footprint"
SUMMARY_KIND = "streaming_footprint_summary"
SIDES = ("with_streaming", "without_streaming")
BUDGETS = ("installed_size", "idle_rss", "steady_rss")

EXIT_PASS = 0
EXIT_FAIL = 1
EXIT_NO_VERDICT = 2


class CaptureError(Exception):
    """A raw capture the record needs is missing or unreadable."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def load_schema(path: Path = SCHEMA_PATH) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def validate_thresholds(thresholds: Any, schema: dict[str, Any] | None = None) -> None:
    schema = load_schema() if schema is None else schema
    validate_record(
        thresholds, {"$ref": "#/definitions/Thresholds", "definitions": schema["definitions"]}
    )


def _display(path: Path) -> str:
    try:
        return str(path.resolve().relative_to(REPO_ROOT))
    except ValueError:
        return path.name


def load_thresholds(path: Path = THRESHOLDS_PATH) -> dict[str, Any]:
    """The declared budgets, refused unless they have the shape the schema requires."""
    try:
        thresholds = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        raise RecordError(f"thresholds file {_display(path)}: {exc}") from exc
    validate_thresholds(thresholds)
    return thresholds


def run_value(budget: str, run: dict[str, Any]) -> tuple[int | None, str | None]:
    """One run's value for a budget in bytes, or why it has none."""
    if budget == "installed_size":
        files = run["elf_files"]
        if not files:
            return None, "no ELF file measured"
        return sum(entry["stripped_bytes"] for entry in files), None
    if budget == "idle_rss":
        samples = run["idle_rss_samples_kib"]
        if not samples:
            return None, "no idle VmRSS sample"
        return max(samples) * 1024, None
    steady = run["steady"]
    if steady["status"] != "measured":
        return None, f"steady state not measured: {steady['reason']}"
    if not steady["samples_kib"]:
        return None, "no steady-state VmRSS sample"
    return max(steady["samples_kib"]) * 1024, None


def derive_result(thresholds: dict[str, Any], sides: dict[str, Any]) -> dict[str, Any]:
    """Per-budget values, maximum deltas and statuses, from the raw measurements only."""
    budgets: dict[str, Any] = {}
    for budget in BUDGETS:
        allowed = thresholds["budgets"][budget]["max_delta_bytes"]
        values: dict[str, list[int]] = {side: [] for side in SIDES}
        reasons: list[str] = []
        for side in SIDES:
            runs = sides[side]["runs"]
            if len(runs) < thresholds["runs"]:
                reasons.append(f"{side}: {len(runs)} of {thresholds['runs']} runs recorded")
            for run in runs:
                value, reason = run_value(budget, run)
                if value is None:
                    reasons.append(f"{side} run {run['index']}: {reason}")
                else:
                    values[side].append(value)
        entry: dict[str, Any] = {
            "with_streaming_bytes": values["with_streaming"],
            "without_streaming_bytes": values["without_streaming"],
            "max_delta_bytes": None,
            "max_delta_bytes_allowed": allowed,
            "status": "incomplete",
            "reason": None,
        }
        if reasons:
            entry["reason"] = "; ".join(reasons)
        else:
            delta = max(values["with_streaming"]) - min(values["without_streaming"])
            entry["max_delta_bytes"] = delta
            entry["status"] = "within" if delta <= allowed else "exceeded"
        budgets[budget] = entry
    statuses = {entry["status"] for entry in budgets.values()}
    if "incomplete" in statuses:
        status = "incomplete"
    elif "exceeded" in statuses:
        status = "fail"
    else:
        status = "pass"
    return {"status": status, "budgets": budgets}


def _first_difference(claimed: Any, derived: Any, pointer: str = "") -> str:
    if isinstance(claimed, dict) and isinstance(derived, dict):
        for key in sorted(set(claimed) | set(derived)):
            if key not in claimed or key not in derived:
                return f"{pointer}/{key}: present on one side only"
            found = _first_difference(claimed[key], derived[key], f"{pointer}/{key}")
            if found:
                return found
        return ""
    if isinstance(claimed, list) and isinstance(derived, list):
        if len(claimed) != len(derived):
            return f"{pointer}: {len(claimed)} items claimed, {len(derived)} derived"
        for index, (left, right) in enumerate(zip(claimed, derived, strict=True)):
            found = _first_difference(left, right, f"{pointer}/{index}")
            if found:
                return found
        return ""
    if claimed != derived:
        return f"{pointer or '/'}: claimed {claimed!r}, derived {derived!r}"
    return ""


def check_record(record: Any, thresholds_path: Path = THRESHOLDS_PATH) -> tuple[str, str]:
    """The record's verdict and why, or `invalid` when the record cannot be trusted."""
    try:
        validate_record(record, load_schema())
    except RecordError as exc:
        return "invalid", f"record does not match its schema: {exc}"
    try:
        canonical = load_thresholds(thresholds_path)
    except RecordError as exc:
        return "invalid", str(exc)
    if record["thresholds"] != canonical:
        found = _first_difference(record["thresholds"], canonical)
        return (
            "invalid",
            f"thresholds in the record differ from {_display(thresholds_path)}: {found}",
        )
    for side in SIDES:
        runs = record["sides"][side]["runs"]
        if len(runs) > canonical["runs"]:
            return "invalid", f"{side}: {len(runs)} runs recorded, {canonical['runs']} declared"
        for position, run in enumerate(runs, start=1):
            if run["index"] != position:
                return "invalid", f"{side}: run {position} carries index {run['index']}"
            steady = run["steady"]
            if steady["status"] == "measured":
                if steady["reason"] is not None:
                    return "invalid", f"{side} run {position}: a measured steady state has a reason"
                if steady["streams"] != canonical["budgets"]["steady_rss"]["streams"]:
                    return (
                        "invalid",
                        f"{side} run {position}: steady state held {steady['streams']} streams,"
                        f" {canonical['budgets']['steady_rss']['streams']} declared",
                    )
            else:
                if not steady["reason"]:
                    return "invalid", f"{side} run {position}: steady state not run with no reason"
                if steady["streams"] is not None or steady["samples_kib"]:
                    return (
                        "invalid",
                        f"{side} run {position}: a steady state not run carries samples",
                    )
    derived = derive_result(canonical, record["sides"])
    if record["result"] != derived:
        found = _first_difference(record["result"], derived)
        return "invalid", f"result differs from the one derived from the samples: {found}"
    status = record["result"]["status"]
    parts = []
    for budget in BUDGETS:
        entry = derived["budgets"][budget]
        if entry["status"] == "incomplete":
            parts.append(f"{budget} incomplete ({entry['reason']})")
        else:
            parts.append(
                f"{budget} {entry['status']} ({entry['max_delta_bytes']} of"
                f" {entry['max_delta_bytes_allowed']} bytes)"
            )
    return status, "; ".join(parts)


def exit_status(status: str) -> int:
    if status == "pass":
        return EXIT_PASS
    if status == "fail":
        return EXIT_FAIL
    return EXIT_NO_VERDICT


# --- assembly from the harness's raw captures ---------------------------------


def _read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as exc:
        raise CaptureError(f"{path}: {exc}") from exc


def _read_pairs(path: Path) -> dict[str, str]:
    pairs: dict[str, str] = {}
    for line in _read_text(path).splitlines():
        if not line.strip():
            continue
        key, _, value = line.partition(" ")
        if not value:
            raise CaptureError(f"{path}: line {line!r} is not 'key value'")
        pairs[key] = value
    return pairs


def _read_samples(path: Path) -> list[int]:
    samples = []
    for number, line in enumerate(_read_text(path).splitlines(), start=1):
        if not line.strip():
            continue
        fields = line.split("\t")
        if len(fields) != 2:
            raise CaptureError(f"{path}:{number}: expected '<time_ns>\\t<kib>'")
        try:
            samples.append(int(fields[1]))
        except ValueError as exc:
            raise CaptureError(f"{path}:{number}: {fields[1]!r} is not an integer") from exc
    return samples


def _read_run(run_dir: Path, index: int) -> dict[str, Any]:
    if not run_dir.is_dir():
        raise CaptureError(f"{run_dir}: run {index} was not captured")
    files = []
    for number, line in enumerate(_read_text(run_dir / "elf-files.tsv").splitlines(), start=1):
        if not line.strip():
            continue
        fields = line.split("\t")
        if len(fields) != 3:
            raise CaptureError(f"{run_dir / 'elf-files.tsv'}:{number}: expected three fields")
        try:
            files.append(
                {"path": fields[0], "bytes": int(fields[1]), "stripped_bytes": int(fields[2])}
            )
        except ValueError as exc:
            raise CaptureError(f"{run_dir / 'elf-files.tsv'}:{number}: {exc}") from exc
    idle = _read_samples(run_dir / "idle-vmrss.tsv")
    measured = run_dir / "steady-vmrss.tsv"
    not_run = run_dir / "steady-not-run.txt"
    if measured.exists() and not_run.exists():
        raise CaptureError(f"{run_dir}: both a steady-state capture and a not-run reason")
    if measured.exists():
        streams_text = _read_text(run_dir / "steady-streams.txt").strip()
        try:
            streams = int(streams_text)
        except ValueError as exc:
            raise CaptureError(f"{run_dir / 'steady-streams.txt'}: {streams_text!r}") from exc
        steady = {
            "status": "measured",
            "reason": None,
            "streams": streams,
            "samples_kib": _read_samples(measured),
        }
    elif not_run.exists():
        reason = _read_text(not_run).strip()
        if not reason:
            raise CaptureError(f"{not_run}: empty reason")
        steady = {"status": "not_run", "reason": reason, "streams": None, "samples_kib": []}
    else:
        raise CaptureError(f"{run_dir}: neither a steady-state capture nor a not-run reason")
    return {"index": index, "elf_files": files, "idle_rss_samples_kib": idle, "steady": steady}


def _read_side(side_dir: Path, runs: int) -> dict[str, Any]:
    package = _read_pairs(side_dir / "package.txt")
    for key in ("file", "sha256", "version", "architecture"):
        if key not in package:
            raise CaptureError(f"{side_dir / 'package.txt'}: missing {key}")
    version_output = _read_text(side_dir / "version-output.txt").splitlines()
    return {
        "package": {key: package[key] for key in ("file", "sha256", "version", "architecture")},
        "version_output": version_output,
        "runs": [_read_run(side_dir / f"run-{index}", index) for index in range(1, runs + 1)],
    }


def assemble(captures: Path, provenance: str, source_commit: str | None) -> dict[str, Any]:
    """The record from a capture directory; any missing capture is an error, never a default."""
    thresholds = json.loads(_read_text(captures / "thresholds.json"))
    validate_thresholds(thresholds)
    host = _read_pairs(captures / "host.txt")
    for key in ("machine", "kernel"):
        if key not in host:
            raise CaptureError(f"{captures / 'host.txt'}: missing {key}")
    sides = {side: _read_side(captures / side, thresholds["runs"]) for side in SIDES}
    return {
        "schema_version": "0.1",
        "record_kind": RECORD_KIND,
        "provenance": provenance,
        "tool": {"name": TOOL_NAME, "source_commit": source_commit},
        "recorded_at_utc": _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "host": {"machine": host["machine"], "kernel": host["kernel"]},
        "thresholds": thresholds,
        "sides": sides,
        "result": derive_result(thresholds, sides),
    }


def summary(record: dict[str, Any], record_path: Path) -> dict[str, Any]:
    """What is published: identity, budgets, deltas and verdict, with the full record's digest."""
    return {
        "schema_version": "0.1",
        "record_kind": SUMMARY_KIND,
        "record_sha256": sha256_file(record_path),
        "provenance": record["provenance"],
        "tool": record["tool"],
        "recorded_at_utc": record["recorded_at_utc"],
        "host": record["host"],
        "thresholds": record["thresholds"],
        "sides": {
            side: {
                "package": record["sides"][side]["package"],
                "version_output": record["sides"][side]["version_output"],
                "runs": len(record["sides"][side]["runs"]),
            }
            for side in SIDES
        },
        "result": record["result"],
    }


def dump(document: dict[str, Any]) -> str:
    return json.dumps(document, indent=2, sort_keys=False) + "\n"


# --- helpers the harness calls ---------------------------------------------------


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def wait_ready(url: str, timeout_s: float) -> tuple[bool, str]:
    """Poll /health until it reports `ready`; the last body or error is returned either way."""
    deadline = time.monotonic() + timeout_s
    last = "no response"
    while True:
        try:
            with urllib.request.urlopen(url, timeout=2) as response:
                body = response.read().decode("utf-8", errors="replace")
            last = body
            try:
                if json.loads(body).get("state") == "ready":
                    return True, body
            except ValueError:
                pass
        except urllib.error.HTTPError as exc:
            # A 503 is a state the worker reports in its body; keep the body, not the status line.
            last = exc.read().decode("utf-8", errors="replace") or str(exc)
        except (urllib.error.URLError, OSError, ValueError) as exc:
            last = str(exc)
        if time.monotonic() >= deadline:
            return False, last
        time.sleep(0.2)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)

    check = commands.add_parser("check", help="verify a record and print its verdict")
    check.add_argument("record", type=Path)

    build = commands.add_parser("assemble", help="build the record from a capture directory")
    build.add_argument("--captures", type=Path, required=True)
    build.add_argument("--out", type=Path, required=True)
    build.add_argument("--provenance", choices=("recorded", "synthetic"), default="recorded")
    build.add_argument("--source-commit", default=None)

    publish = commands.add_parser("summary", help="write the publishable summary of a record")
    publish.add_argument("record", type=Path)
    publish.add_argument("--out", type=Path, required=True)

    commands.add_parser("free-port", help="print a free loopback TCP port")

    ready = commands.add_parser("wait-ready", help="poll a worker's /health until it is ready")
    ready.add_argument("--url", required=True)
    ready.add_argument("--timeout", type=float, required=True)
    ready.add_argument("--out", type=Path, required=True)

    args = parser.parse_args(argv)
    if args.command == "check":
        try:
            record = json.loads(args.record.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            print(f"{TOOL_NAME}: invalid: {args.record}: {exc}", file=sys.stderr)
            return EXIT_NO_VERDICT
        status, detail = check_record(record)
        stream = sys.stdout if status == "pass" else sys.stderr
        if status != "invalid" and record.get("provenance") == "synthetic":
            print(f"{TOOL_NAME}: provenance synthetic: these numbers measure nothing", file=stream)
        print(f"{TOOL_NAME}: {status}: {detail}", file=stream)
        return exit_status(status)
    if args.command == "assemble":
        try:
            record = assemble(args.captures, args.provenance, args.source_commit)
        except (CaptureError, RecordError, ValueError) as exc:
            print(f"{TOOL_NAME}: cannot assemble a record: {exc}", file=sys.stderr)
            return EXIT_NO_VERDICT
        args.out.write_text(dump(record), encoding="utf-8")
        return 0
    if args.command == "summary":
        try:
            record = json.loads(args.record.read_text(encoding="utf-8"))
            status, detail = check_record(record)
        except (OSError, ValueError) as exc:
            print(f"{TOOL_NAME}: invalid: {args.record}: {exc}", file=sys.stderr)
            return EXIT_NO_VERDICT
        if status == "invalid":
            print(f"{TOOL_NAME}: {status}: {detail}", file=sys.stderr)
            return EXIT_NO_VERDICT
        args.out.write_text(dump(summary(record, args.record)), encoding="utf-8")
        return 0
    if args.command == "free-port":
        print(free_port())
        return 0
    ok, body = wait_ready(args.url, args.timeout)
    args.out.write_text(body if body.endswith("\n") else body + "\n", encoding="utf-8")
    if not ok:
        print(f"{TOOL_NAME}: worker not ready after {args.timeout}s: {body}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
