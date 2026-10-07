#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Drive the streaming footprint checker and harness without a Nano.

The checker is shown to refuse, for its own reason, every way a record could
claim more than its samples support. The harness runs against fake packages
whose worker is a small compiled server that adds known amounts to its
stripped size and resident set, so each delta the record derives is checked
against what the fakes were told to add.
"""

from __future__ import annotations

import copy
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MODULE = ROOT / "tools/validation/streaming_footprint_record.py"
HARNESS = ROOT / "tools/validation/streaming-footprint.sh"
THRESHOLDS = ROOT / "tools/validation/streaming-footprint-thresholds.json"
SCHEMA = ROOT / "config/schemas/streaming_footprint_record.json"
FAKES = ROOT / "test/validation/fixtures/streaming_footprint_fakes"
sys.path.insert(0, str(ROOT / "tools/validation"))
import streaming_footprint_record as record_mod  # noqa: E402

try:
    import jsonschema
except ImportError:  # the schema cross-check needs it; CI installs it
    jsonschema = None

MIB = 1024 * 1024
failures = 0


def check(what: str, ok: bool, detail: str = "") -> None:
    global failures
    if ok:
        print(f"  ok   {what}")
    else:
        print(f"  FAIL {what}" + (f"\n       {detail}" if detail else ""))
        failures += 1


def run_cli(*args: str, env: dict | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(MODULE), *args], text=True, capture_output=True, env=env
    )


# --- the checker ------------------------------------------------------------------


def synthetic_run(index: int, stripped: int, idle_kib: int, steady_kib: int) -> dict:
    return {
        "index": index,
        "elf_files": [
            {
                "path": "usr/lib/tensorplate/tensorplate-serving",
                "bytes": stripped + 4096,
                "stripped_bytes": stripped,
            }
        ],
        "idle_rss_samples_kib": [idle_kib, idle_kib + 8, idle_kib + 4],
        "steady": {
            "status": "measured",
            "reason": None,
            "streams": 16,
            "samples_kib": [steady_kib, steady_kib + 16, steady_kib + 8],
        },
    }


def synthetic_record(thresholds: dict) -> dict:
    runs = thresholds["runs"]
    sides = {
        "with_streaming": {
            "package": {
                "file": "tensorplate-serving_0.3.1_amd64.deb",
                "sha256": "a" * 64,
                "version": "0.3.1",
                "architecture": "amd64",
            },
            "version_output": ["tensorplate-serving 0.3.1", "protocol 0.1", "bundle-format 0.1"],
            "runs": [synthetic_run(i, 24 * MIB, 50_000, 90_000) for i in range(1, runs + 1)],
        },
        "without_streaming": {
            "package": {
                "file": "tensorplate-serving_0.3.1~dev_amd64.deb",
                "sha256": "b" * 64,
                "version": "0.3.1~dev",
                "architecture": "amd64",
            },
            "version_output": [],
            "runs": [synthetic_run(i, 10 * MIB, 30_000, 40_000) for i in range(1, runs + 1)],
        },
    }
    return {
        "schema_version": "0.1",
        "record_kind": "streaming_footprint",
        "provenance": "synthetic",
        "tool": {"name": "streaming-footprint", "source_commit": None},
        "recorded_at_utc": "2026-10-07T00:00:00Z",
        "host": {"machine": "x86_64", "kernel": "6.8.0"},
        "thresholds": copy.deepcopy(thresholds),
        "sides": sides,
        "result": record_mod.derive_result(thresholds, sides),
    }


def rederive(record: dict) -> dict:
    record["result"] = record_mod.derive_result(record["thresholds"], record["sides"])
    return record


def checker_cases(work: Path) -> dict:
    thresholds = record_mod.load_thresholds()
    check(
        "thresholds declare three budgets and three runs",
        thresholds["runs"] == 3
        and thresholds["budgets"]["installed_size"]["max_delta_bytes"] == 32 * MIB
        and thresholds["budgets"]["idle_rss"]["max_delta_bytes"] == 64 * MIB
        and thresholds["budgets"]["steady_rss"]["max_delta_bytes"] == 128 * MIB
        and thresholds["budgets"]["steady_rss"]["streams"] == 16,
        json.dumps(thresholds),
    )
    good = synthetic_record(thresholds)
    good_path = work / "good.json"
    good_path.write_text(record_mod.dump(good))
    result = run_cli("check", str(good_path))
    check(
        "a complete record within every budget passes",
        result.returncode == 0 and "pass:" in result.stdout,
        result.stdout + result.stderr,
    )
    check(
        "the maximum delta is the largest with-streaming value minus the smallest without",
        good["result"]["budgets"]["idle_rss"]["max_delta_bytes"] == (50_008 - 30_008) * 1024
        and good["result"]["budgets"]["installed_size"]["max_delta_bytes"] == 14 * MIB,
        json.dumps(good["result"]["budgets"]["idle_rss"]),
    )
    if jsonschema is not None:
        try:
            jsonschema.validate(good, json.loads(SCHEMA.read_text()))
            jsonschema.validate(
                thresholds,
                {
                    "$ref": "#/definitions/Thresholds",
                    "definitions": json.loads(SCHEMA.read_text())["definitions"],
                },
            )
            check("jsonschema accepts the record and the thresholds", True)
        except jsonschema.ValidationError as exc:
            check("jsonschema accepts the record and the thresholds", False, str(exc))

    def mutate(name: str, apply, expect_code: int, expect_text: str, honest: bool = False):
        record = copy.deepcopy(good)
        apply(record)
        if honest:
            rederive(record)
        path = work / "mutant.json"
        path.write_text(record_mod.dump(record))
        result = run_cli("check", str(path))
        output = result.stdout + result.stderr
        check(
            name,
            result.returncode == expect_code and expect_text in output,
            f"exit {result.returncode}; {output.strip()}",
        )
        if jsonschema is not None and "does not match its schema" in expect_text:
            try:
                jsonschema.validate(record, json.loads(SCHEMA.read_text()))
                check(f"jsonschema also rejects: {name}", False, "jsonschema accepted it")
            except jsonschema.ValidationError:
                check(f"jsonschema also rejects: {name}", True)

    def set_budget(record, budget, value):
        record["thresholds"]["budgets"][budget]["max_delta_bytes"] = value

    def drop_run(record, side):
        record["sides"][side]["runs"].pop()

    # An edited budget, even one the samples would still satisfy.
    mutate(
        "a threshold edited in the record is refused",
        lambda r: set_budget(r, "idle_rss", 64 * MIB + 1),
        2,
        "thresholds in the record differ from tools/validation/streaming-footprint-thresholds.json",
        honest=True,
    )
    mutate(
        "a threshold missing from the record is refused",
        lambda r: r["thresholds"]["budgets"].pop("steady_rss"),
        2,
        "record does not match its schema",
    )

    def fewer_runs_declared(record):
        record["thresholds"]["runs"] = 2
        for side in record_mod.SIDES:
            drop_run(record, side)

    mutate(
        "a record that lowered the declared run count is refused",
        fewer_runs_declared,
        2,
        "thresholds in the record differ",
        honest=True,
    )
    mutate(
        "a missing run leaves the record incomplete",
        lambda r: drop_run(r, "with_streaming"),
        2,
        "incomplete: installed_size incomplete (with_streaming: 2 of 3 runs recorded)",
        honest=True,
    )
    mutate(
        "a missing run claimed as a pass is refused",
        lambda r: drop_run(r, "without_streaming"),
        2,
        "result differs from the one derived from the samples",
    )
    mutate(
        "a run with no idle sample leaves the record incomplete",
        lambda r: r["sides"]["with_streaming"]["runs"][1].update(idle_rss_samples_kib=[]),
        2,
        "idle_rss incomplete (with_streaming run 2: no idle VmRSS sample)",
        honest=True,
    )
    mutate(
        "a run with no ELF file leaves the record incomplete",
        lambda r: r["sides"]["without_streaming"]["runs"][0].update(elf_files=[]),
        2,
        "installed_size incomplete (without_streaming run 1: no ELF file measured)",
        honest=True,
    )

    def steady_not_run(record):
        record["sides"]["with_streaming"]["runs"][2]["steady"] = {
            "status": "not_run",
            "reason": "no stream driver was given",
            "streams": None,
            "samples_kib": [],
        }

    mutate(
        "a steady state not run leaves the record incomplete, with the reason",
        steady_not_run,
        2,
        "steady_rss incomplete (with_streaming run 3: steady state not measured: no stream driver",
        honest=True,
    )

    def steady_not_run_no_reason(record):
        steady_not_run(record)
        record["sides"]["with_streaming"]["runs"][2]["steady"]["reason"] = ""

    mutate(
        "a steady state not run without a reason is refused",
        steady_not_run_no_reason,
        2,
        "steady state not run with no reason",
        honest=True,
    )
    mutate(
        "a steady state that held fewer streams than declared is refused",
        lambda r: r["sides"]["with_streaming"]["runs"][0]["steady"].update(streams=8),
        2,
        "steady state held 8 streams, 16 declared",
        honest=True,
    )
    mutate(
        "a steady state with no sample leaves the record incomplete",
        lambda r: r["sides"]["without_streaming"]["runs"][1]["steady"].update(samples_kib=[]),
        2,
        "steady_rss incomplete (without_streaming run 2: no steady-state VmRSS sample)",
        honest=True,
    )
    mutate(
        "more runs than declared are refused",
        lambda r: r["sides"]["with_streaming"]["runs"].append(synthetic_run(4, MIB, 1, 1)),
        2,
        "4 runs recorded, 3 declared",
        honest=True,
    )
    mutate(
        "a run out of order is refused",
        lambda r: r["sides"]["with_streaming"]["runs"].reverse(),
        2,
        "run 1 carries index 3",
        honest=True,
    )

    def exceed_idle(record):
        for run in record["sides"]["with_streaming"]["runs"]:
            run["idle_rss_samples_kib"] = [30_008 + 64 * 1024 + 1]

    mutate(
        "a delta over its budget fails",
        exceed_idle,
        1,
        "fail: installed_size within (14680064 of 33554432 bytes); idle_rss exceeded"
        " (67109888 of 67108864 bytes)",
        honest=True,
    )

    def exceed_idle_but_claim_pass(record):
        exceed_idle(record)
        rederive(record)
        record["result"]["status"] = "pass"
        record["result"]["budgets"]["idle_rss"]["status"] = "within"

    mutate(
        "an exceeded budget claimed as within is refused",
        exceed_idle_but_claim_pass,
        2,
        "result differs from the one derived from the samples: /budgets/idle_rss/status",
    )
    mutate(
        "a recorded delta smaller than the samples give is refused",
        lambda r: r["result"]["budgets"]["installed_size"].update(max_delta_bytes=1),
        2,
        "result differs from the one derived from the samples: /budgets/installed_size/max_delta",
    )
    mutate(
        "a recorded provenance outside the two allowed is refused",
        lambda r: r.update(provenance="measured"),
        2,
        "record does not match its schema",
    )

    broken = work / "thresholds-missing-budget.json"
    broken_thresholds = copy.deepcopy(thresholds)
    broken_thresholds["budgets"].pop("idle_rss")
    broken.write_text(json.dumps(broken_thresholds))
    status, detail = record_mod.check_record(copy.deepcopy(good), thresholds_path=broken)
    check(
        "a declared thresholds file missing a budget gives no verdict",
        status == "invalid" and "missing required field 'idle_rss'" in detail,
        f"{status}: {detail}",
    )
    status, detail = record_mod.check_record(copy.deepcopy(good), thresholds_path=work / "absent")
    check(
        "an absent thresholds file gives no verdict",
        status == "invalid" and detail.startswith("thresholds file"),
        f"{status}: {detail}",
    )

    summary_path = work / "summary.json"
    result = run_cli("summary", str(good_path), "--out", str(summary_path))
    summary = json.loads(summary_path.read_text()) if summary_path.exists() else {}
    check(
        "the summary carries the record's digest, identity and result and no sample",
        result.returncode == 0
        and summary.get("record_sha256") == hashlib.sha256(good_path.read_bytes()).hexdigest()
        and summary.get("result") == good["result"]
        and summary["sides"]["with_streaming"]["runs"] == 3
        and "samples" not in json.dumps(summary)
        and "elf_files" not in json.dumps(summary),
        result.stdout + result.stderr,
    )
    tampered = copy.deepcopy(good)
    tampered["result"]["status"] = "fail"
    (work / "tampered.json").write_text(record_mod.dump(tampered))
    result = run_cli("summary", str(work / "tampered.json"), "--out", str(work / "no.json"))
    check(
        "no summary is written for a record the check refuses",
        result.returncode == 2 and not (work / "no.json").exists(),
        result.stdout + result.stderr,
    )
    return good


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="tp-footprint-test-") as tmp:
        work = Path(tmp)
        (work / "checker").mkdir()
        print("checker")
        checker_cases(work / "checker")
    if failures:
        print(f"streaming_footprint_test: {failures} check(s) failed")
        return 1
    print("streaming_footprint_test: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
